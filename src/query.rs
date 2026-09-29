//! Pagination partagée par REST (feature 4), et plus tard GraphQL/MCP — volontairement pas dans
//! `rest/`, pour éviter de la dupliquer quand ces autres couches en auront besoin.

/// Valeur par défaut de `per_page` quand le paramètre est absent.
pub const DEFAULT_PER_PAGE: u64 = 100;
/// Plafond absolu de `per_page`, pour empêcher qu'une liste ne remonte des milliers de
/// lignes d'un coup.
pub const MAX_PER_PAGE: u64 = 1000;
/// Plafond de sûreté de `page` (arbitré 2026-09-27) : pour tout `per_page` valide
/// (`1..=MAX_PER_PAGE`), `per_page * page` ne déborde jamais un `u64` chez l'appelant.
pub const MAX_PAGE: u64 = u64::MAX / MAX_PER_PAGE;

/// Paramètres de pagination normalisés — l'objectif n'est pas une pagination fine, juste
/// d'empêcher qu'une liste ne remonte des milliers de lignes d'un coup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pagination {
    /// 1-indexée côté API.
    pub page: u64,
    /// Nombre d'éléments par page — défaut `DEFAULT_PER_PAGE` quand le paramètre est absent,
    /// écrêté à `[1, MAX_PER_PAGE]` indépendamment de `page`.
    pub per_page: u64,
}

impl Pagination {
    /// Construit depuis des query params bruts, potentiellement absents ou hors bornes.
    /// `page` clampée à `[1, MAX_PAGE]`, `per_page` clampée à `[1, MAX_PER_PAGE]`,
    /// indépendamment l'une de l'autre.
    #[must_use]
    pub fn from_raw(page: Option<u64>, per_page: Option<u64>) -> Self {
        let page = page.unwrap_or(1).clamp(1, MAX_PAGE);
        let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
        Self { page, per_page }
    }
}

/// Page de résultats, avec assez de métadonnées pour qu'un client sache s'il en reste d'autres.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PagedResult<M> {
    /// Éléments de la page demandée — un `Vec` vide se sérialise `[]`, jamais `null` ; page
    /// hors bornes : vide, sans recalcul.
    pub items: Vec<M>,
    /// Page rendue, 1-indexée — simple écho de la demande : une page au-delà de la dernière
    /// n'est jamais ramenée à `total_pages`.
    pub page: u64,
    /// Taille de page effectivement utilisée, écho de la normalisation `Pagination`.
    pub per_page: u64,
    /// Total d'éléments de la collection — rempli chez l'appelant depuis `SeaORM`
    /// (`num_items_and_pages`), jamais recalculé ni revérifié ici.
    pub total_items: u64,
    /// Total de pages (division majorante amont) — `0` ligne donne `0` page, pas `1`.
    pub total_pages: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Scenario « paramètres absents — valeurs par défaut ».
    #[test]
    fn defaults_when_absent() {
        let p = Pagination::from_raw(None, None);
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, DEFAULT_PER_PAGE);
    }

    /// Scenario « `per_page` au-dessus du plafond écrêté au maximum ».
    #[test]
    fn per_page_clamped_to_max() {
        let p = Pagination::from_raw(None, Some(50_000));
        assert_eq!(p.per_page, MAX_PER_PAGE);
    }

    /// Scenario « `per_page` à zéro remonté au minimum ».
    #[test]
    fn per_page_clamped_to_min() {
        let p = Pagination::from_raw(None, Some(0));
        assert_eq!(p.per_page, 1);
    }

    /// Scenario « `page` à zéro ramenée à la première page » — `per_page` doit rester
    /// intacté, l'écrêtement de `page` ne le touche pas.
    #[test]
    fn page_clamped_to_min() {
        let p = Pagination::from_raw(Some(0), None);
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, DEFAULT_PER_PAGE);
    }

    /// Scenario « `per_page` aux bornes exactes traversé sans changement » : `1` n'est
    /// pas remplacé par le défaut `100`, `MAX_PER_PAGE` inclus passe inchangé.
    #[test]
    fn per_page_exact_bounds_pass_through() {
        let p = Pagination::from_raw(None, Some(1));
        assert_eq!(p.per_page, 1);
        let p = Pagination::from_raw(None, Some(MAX_PER_PAGE));
        assert_eq!(p.per_page, MAX_PER_PAGE);
    }

    /// Scenario « paramètres valides transmis tels quels » : l'intérieur de la fenêtre
    /// n'est jamais déplacé.
    #[test]
    fn valid_parameters_pass_through() {
        let p = Pagination::from_raw(Some(3), Some(25));
        assert_eq!(p.page, 3);
        assert_eq!(p.per_page, 25);
    }

    /// Scenario « `page` extrême ramenée au plafond de sûreté » : `u64::MAX` est ramenée
    /// au plafond `MAX_PAGE` (`u64::MAX / MAX_PER_PAGE`), garantissant que
    /// `per_page * page` ne déborde jamais chez l'appelant.
    #[test]
    fn page_clamped_to_max() {
        let p = Pagination::from_raw(Some(u64::MAX), None);
        assert_eq!(p.page, MAX_PAGE);
        assert_eq!(p.per_page, DEFAULT_PER_PAGE);
    }

    /// Scenario « forme JSON stable de `PagedResult` » : exactement les cinq clés wire,
    /// valeurs telles que fournies.
    #[test]
    fn paged_result_json_shape() {
        let result = PagedResult {
            items: vec![7u8, 8],
            page: 2,
            per_page: 25,
            total_items: 30,
            total_pages: 2,
        };
        let value = serde_json::to_value(&result).expect("PagedResult is serializable");
        let object = value.as_object().expect("PagedResult serializes to an object");
        assert_eq!(object.len(), 5);
        assert_eq!(object["items"], json!([7, 8]));
        assert_eq!(object["page"], json!(2));
        assert_eq!(object["per_page"], json!(25));
        assert_eq!(object["total_items"], json!(30));
        assert_eq!(object["total_pages"], json!(2));
    }

    /// Scenario « table vide — total nul sérialisé à zéro » : `items` rend `[]` et non
    /// `null`, une collection vide fait `0` page, pas `1`.
    #[test]
    fn empty_result_serializes_zero_totals() {
        let result = PagedResult {
            items: Vec::<u8>::new(),
            page: 1,
            per_page: DEFAULT_PER_PAGE,
            total_items: 0,
            total_pages: 0,
        };
        let value = serde_json::to_value(&result).expect("PagedResult is serializable");
        assert_eq!(value["items"], json!([]));
        assert!(value["items"].is_array());
        assert!(!value["items"].is_null());
        assert_eq!(value["total_items"], json!(0));
        assert_eq!(value["total_pages"], json!(0));
    }

    /// Scenario « `page` au-delà de la dernière — items vides et totaux préservés » :
    /// le DTO est un écho pur, rien n'est recalculé ni ramené à la dernière page.
    #[test]
    fn out_of_range_page_keeps_totals() {
        let result = PagedResult {
            items: Vec::<u8>::new(),
            page: 99,
            per_page: 10,
            total_items: 30,
            total_pages: 3,
        };
        let value = serde_json::to_value(&result).expect("PagedResult is serializable");
        assert_eq!(value["items"], json!([]));
        assert_eq!(value["page"], json!(99));
        assert_eq!(value["per_page"], json!(10));
        assert_eq!(value["total_items"], json!(30));
        assert_eq!(value["total_pages"], json!(3));
    }
}
