//! Pagination partagée par REST (feature 4), et plus tard GraphQL/MCP — volontairement pas dans
//! `rest/`, pour éviter de la dupliquer quand ces autres couches en auront besoin.

pub const DEFAULT_PER_PAGE: u64 = 100;
pub const MAX_PER_PAGE: u64 = 1000;

/// Paramètres de pagination normalisés — l'objectif n'est pas une pagination fine, juste
/// d'empêcher qu'une liste ne remonte des milliers de lignes d'un coup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pagination {
    /// 1-indexée côté API.
    pub page: u64,
    pub per_page: u64,
}

impl Pagination {
    /// Construit depuis des query params bruts, potentiellement absents ou hors bornes.
    /// `page` clampée à un minimum de 1, `per_page` clampée à `[1, MAX_PER_PAGE]`.
    pub fn from_raw(page: Option<u64>, per_page: Option<u64>) -> Self {
        let page = page.unwrap_or(1).max(1);
        let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
        Self { page, per_page }
    }
}

/// Page de résultats, avec assez de métadonnées pour qu'un client sache s'il en reste d'autres.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PagedResult<M> {
    pub items: Vec<M>,
    pub page: u64,
    pub per_page: u64,
    pub total_items: u64,
    pub total_pages: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_absent() {
        let p = Pagination::from_raw(None, None);
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, DEFAULT_PER_PAGE);
    }

    #[test]
    fn per_page_clamped_to_max() {
        let p = Pagination::from_raw(None, Some(50_000));
        assert_eq!(p.per_page, MAX_PER_PAGE);
    }

    #[test]
    fn per_page_clamped_to_min() {
        let p = Pagination::from_raw(None, Some(0));
        assert_eq!(p.per_page, 1);
    }

    #[test]
    fn page_clamped_to_min() {
        let p = Pagination::from_raw(Some(0), None);
        assert_eq!(p.page, 1);
    }
}
