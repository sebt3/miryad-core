//! Mise en forme des sorties d'agent pour Matrix (`src/lymaille/chat/render.sdd`).
//!
//! Partie **pure** de la surface Matrix (DSD `FR-13`, `DEC-26`) : découpage des réponses longues
//! sur des frontières naturelles (bloc de code fermé, ligne vide, fin de ligne, espace), jamais
//! au milieu d'un bloc de code ouvert (refermé puis rouvert, même langage), numérotation `(i/n)`
//! et troncature au-delà de [`MAX_PARTS`]. Aucun I/O, aucune horloge.

/// Longueur maximale d'une partie, préfixe de numérotation compris (`Must` de `./render.sdd`).
pub const MAX_PART_CHARS: usize = 4000;

/// Nombre maximal de parties envoyées pour une même sortie ; au-delà, la dernière partie est
/// tronquée et l'indique (`Must` de `./render.sdd`).
pub const MAX_PARTS: usize = 8;

/// Réserve pour le préfixe `(i/n) ` : le plus long préfixe possible fait six caractères
/// (`(8/8) `), la réserve arrondit à la largeur des plus longs.
const PREFIX_RESERVE: usize = 8;

/// Réserve pour les marqueurs de bloc de code qu'une coupure peut injecter de part et d'autre
/// d'une frontière : la fermeture du bloc (quatre caractères) et sa réouverture avec le langage
/// (huit caractères avec un langage court).
const FENCE_RESERVE: usize = 16;

/// Réserve pour le suffixe de troncature `… (sortie tronquée, N caractères omis)` mesurée sur la
/// borne haute de `N` (six chiffres sous la borne de texte raisonnable, marge comprise).
const TRUNC_SUFFIX_RESERVE: usize = 48;

/// Corps utile d'une partie : budget `MAX_PART_CHARS` amputé des réserves de préfixe et de
/// marqueurs — le contenu brut que la découpe peut remplir avant toute marque ajoutée.
const BODY_BUDGET: usize = MAX_PART_CHARS - PREFIX_RESERVE - FENCE_RESERVE;

/// Une ligne du texte, avec ses propriétés de découpe (`Must` de `./render.sdd`) : bornes en
/// indices de `char`, ligne vide hors bloc (candidat « ligne vide »), ouverture ou fermeture de
/// bloc de code (la ligne de fermeture est un candidat de première préférence).
struct LineInfo {
    /// Index de premier caractère de la ligne.
    start: usize,
    /// Index après le dernier caractère de la ligne, saut de ligne inclus.
    end: usize,
    /// Ligne vide hors bloc de code — frontière de préférence deux.
    blank: bool,
    /// La ligne ferme un bloc de code ouvert — frontière de préférence une.
    closes: bool,
    /// La ligne ouvre un bloc de code ; la valeur est le langage après ```` ``` ````.
    opens: Option<String>,
}

/// Indexe les lignes du texte et leur état de bloc de code : une ligne dont le contenu (tondu)
/// commence par ```` ``` ```` ouvre un bloc si aucun n'est ouvert, le ferme sinon ; une ligne
/// vide **hors** bloc est candidate à la coupe.
fn line_infos(chars: &[char]) -> Vec<LineInfo> {
    let mut lines = Vec::new();
    let mut in_block = false;
    let mut index = 0usize;
    while index < chars.len() {
        let mut newline = index;
        while chars.get(newline).is_some_and(|caractere| *caractere != '\n') {
            newline = newline.saturating_add(1);
        }
        let end = if newline < chars.len() {
            newline.saturating_add(1)
        } else {
            newline
        };
        let content: String = chars.get(index..newline).unwrap_or(&[]).iter().collect();
        let trimmed = content.trim();
        let mut opens = None;
        let mut closes = false;
        if trimmed.starts_with("```") {
            if in_block {
                closes = true;
                in_block = false;
            } else {
                opens = Some(trimmed.trim_start_matches('`').to_owned());
                in_block = true;
            }
        }
        let blank = !in_block && !closes && opens.is_none() && content.is_empty();
        lines.push(LineInfo {
            start: index,
            end,
            blank,
            closes,
            opens,
        });
        index = end;
    }
    lines
}

/// État du bloc de code à la position `cut` : un bloc est ouvert en `cut`, et son langage.
fn fence_state(lines: &[LineInfo], cut: usize) -> (bool, String) {
    let mut open = false;
    let mut lang = String::new();
    for line in lines {
        if line.start >= cut {
            break;
        }
        if let Some(name) = &line.opens {
            if !open {
                open = true;
                lang.clone_from(name);
            }
        } else if line.closes {
            open = false;
        }
        if cut < line.end {
            break; // coupure en pleine ligne : l'état est celui porté par cette ligne
        }
    }
    (open, lang)
}

/// Position de coupe dans la fenêtre `[from, to)` (`Must` de `./render.sdd`) : fin de bloc de
/// code fermé, puis ligne vide hors bloc, puis fin de ligne, puis espace, puis caractère — à
/// chaque préférence, la position la plus à droite de la fenêtre. La position rendue est
/// strictement supérieure à `from` (progrès garanti).
fn find_cut(lines: &[LineInfo], chars: &[char], from: usize, to: usize) -> usize {
    let mut best = None;
    for line in lines {
        if line.start >= to {
            break;
        }
        if line.closes && line.end > from && line.end <= to {
            best = Some(line.end);
        }
    }
    if let Some(cut) = best {
        return cut;
    }
    best = None;
    for line in lines {
        if line.start >= to {
            break;
        }
        if line.blank && line.end > from && line.end <= to {
            best = Some(line.end);
        }
    }
    if let Some(cut) = best {
        return cut;
    }
    best = None;
    for line in lines {
        if line.start >= to {
            break;
        }
        let candidate = line.end.min(to);
        if candidate > from && chars.get(candidate.saturating_sub(1)).copied() == Some('\n') {
            best = Some(candidate);
        }
    }
    if let Some(cut) = best {
        return cut;
    }
    let window = chars.get(from..to).unwrap_or(&[]);
    if let Some(offset) = window.iter().rposition(|c| *c == ' ')
        && let Some(space) = from.checked_add(offset)
    {
        return space.saturating_add(1);
    }
    to
}

/// Découpe une sortie d'agent en messages Matrix (`./render.sdd`) : vide → vecteur vide ; sous la
/// limite → une partie sans numérotation ; sinon des parties numérotées `(i/n)`, aucune au-delà de
/// `MAX_PART_CHARS`, un bloc de code traversé est refermé en fin de partie et rouvert (même
/// langage) en début de la suivante, et au-delà de `MAX_PARTS` la dernière partie est tronquée et
/// cite le nombre de caractères omis.
pub fn split_message(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Vec::new();
    }
    if total <= MAX_PART_CHARS {
        return vec![text.to_owned()];
    }
    let lines = line_infos(&chars);

    let mut bodies: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let mut consumed = 0usize;
    let mut reopen_prefix = String::new();
    while pos < total {
        let mut body_room = BODY_BUDGET.saturating_sub(reopen_prefix.chars().count());
        let last_allowed = bodies.len().saturating_add(1) == MAX_PARTS;
        if last_allowed && pos.saturating_add(body_room) < total {
            // dernière partie avant troncature : le suffixe « … (sortie tronquée, …) » doit tenir
            // dans le budget, préfixe de numérotation compris.
            body_room = body_room.saturating_sub(TRUNC_SUFFIX_RESERVE);
        }
        if pos.saturating_add(body_room) >= total && !last_allowed {
            let mut part = String::new();
            part.push_str(&reopen_prefix);
            part.push_str(&chars.get(pos..total).unwrap_or(&[]).iter().collect::<String>());
            bodies.push(part);
            break;
        }
        let to = pos.saturating_add(body_room).min(total);
        let mut cut = find_cut(&lines, &chars, pos, to);
        if cut <= pos {
            cut = pos.saturating_add(1);
        }
        let body: String = chars.get(pos..cut).unwrap_or(&[]).iter().collect();
        let (open_at_cut, lang) = fence_state(&lines, cut);
        let mut part = String::new();
        part.push_str(&reopen_prefix);
        part.push_str(&body);
        let mut next_reopen = String::new();
        if open_at_cut {
            part.push_str("\n```");
            next_reopen = format!("```{lang}\n");
        }
        consumed = consumed.saturating_add(cut.saturating_sub(pos));
        bodies.push(part);
        reopen_prefix = next_reopen;
        pos = cut;
        if bodies.len() == MAX_PARTS {
            if pos < total {
                let omis = total.saturating_sub(consumed);
                let suffix = format!("… (sortie tronquée, {omis} caractères omis)");
                if let Some(last) = bodies.last_mut() {
                    last.push_str(&suffix);
                }
            }
            break;
        }
    }

    let n = bodies.len();
    if n == 1 {
        return bodies;
    }
    bodies
        .into_iter()
        .enumerate()
        .map(|(index, body)| format!("({}/{n}) {body}", index.saturating_add(1)))
        .collect()
}

/// Rend les avis de reprise (`OI-8`) en une ligne `Reprise : …` ; vide → `None`
/// (`./render.sdd`).
#[must_use]
pub fn format_notice(notices: &[String]) -> Option<String> {
    if notices.is_empty() {
        return None;
    }
    Some(format!("Reprise : {}", notices.join(" ; ")))
}

/// Message fixe d'un tour en échec : le code `MRD-*`, jamais le texte d'une erreur interne
/// (`./render.sdd`).
#[must_use]
pub fn format_error(code: &str) -> String {
    format!("Échec du tour (code {code}).")
}

/// Aide des commandes du pont (`./bridge.sdd`, R13 : `!new <contexte> [agent]` remplace
/// `!agent`).
#[must_use]
pub fn help_text() -> String {
    "Commandes du pont :\n\
     !new <contexte> [agent] — fermer la session de la salle et en ouvrir une nouvelle\n\
     !cancel — interrompre le tour en cours\n\
     !resume — reprendre la session et ses avis de provenance\n\
     !activate <toolset> — promouvoir le brouillon d'un toolset\n\
     !link <code> — lier ce compte Matrix à un compte miryad\n\
     !help — cette aide"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{MAX_PART_CHARS, MAX_PARTS};
    use super::{format_error, format_notice, help_text, split_message};

    /// Scenario « un texte court reste entier » : 100 caractères → une partie sans numérotation.
    #[test]
    fn un_texte_court_reste_entier() {
        let texte = "a".repeat(100);
        let parties = split_message(&texte);
        assert_eq!(parties.len(), 1, "un texte court est une seule partie");
        assert_eq!(parties[0], texte, "aucune numérotation sous la limite");
    }

    /// Scenario « un texte long est découpé sur une ligne vide » : 9000 caractères en
    /// paragraphes → trois parties numérotées `(1/3)` à `(3/3)`, chacune ≤ 4000, coupures sur
    /// lignes vides.
    #[test]
    fn un_texte_long_est_decoupe_sur_une_ligne_vide() {
        let paragraphe = "a".repeat(300);
        let bloc = format!("{paragraphe}\n{paragraphe}\n"); // 602 caractères par bloc
        let mut texte = String::new();
        while texte.chars().count() < 9000 {
            texte.push_str(&bloc);
            texte.push('\n');
        }
        let parties = split_message(&texte);
        assert_eq!(
            parties.len(),
            3,
            "9000 caractères → trois parties, pas {}",
            parties.len()
        );
        for (rang, partie) in parties.iter().enumerate() {
            let attendu = format!("({}/{}) ", rang + 1, parties.len());
            assert!(
                partie.starts_with(&attendu),
                "préfixe {attendu} attendu : {}",
                &partie[..30.min(partie.len())]
            );
            assert!(
                partie.chars().count() <= MAX_PART_CHARS,
                "partie {rang} > {MAX_PART_CHARS}"
            );
        }
    }

    /// Scenario « un bloc de code traversant une coupure est refermé puis rouvert » : un bloc
    /// ```rust de 6000 caractères → chaque partie a un nombre pair de ``` et la seconde rouvre
    /// ```rust.
    #[test]
    fn un_bloc_de_code_traversant_une_coupure_est_referme_puis_rouvert() {
        let mut texte = String::from("```rust\n");
        while texte.chars().count() < 5990 {
            texte.push_str("let x = 1;\n");
        }
        texte.push_str("```");
        let parties = split_message(&texte);
        assert!(
            parties.len() >= 2,
            "6000 caractères coupent le bloc : {} parties",
            parties.len()
        );
        for (rang, partie) in parties.iter().enumerate() {
            let corps = partie
                .strip_prefix(&format!("({}/{}) ", rang + 1, parties.len()))
                .or(if parties.len() == 1 {
                    Some(partie.as_str())
                } else {
                    None
                })
                .unwrap_or_else(|| panic!("préfixe de numérotation manquant sur la partie {rang}"));
            let clotures = corps.matches("```").count();
            assert_eq!(
                clotures % 2,
                0,
                "partie {rang} : nombre impair de ``` ({clotures})"
            );
        }
        assert!(
            parties[1].starts_with(&format!("(2/{}) ```rust", parties.len())),
            "la seconde partie rouvre ```rust : {}",
            &parties[1][..40.min(parties[1].len())]
        );
    }

    /// Scenario « ne jamais couper un caractère multi-octets » : 5000 `é` sans espace → aucune
    /// panique, toutes les parties sont de l'UTF-8 valide (la découpe en `char` le garantit par
    /// construction ; le test verrouille le contenu reconstruit).
    #[test]
    fn ne_jamais_couper_un_caractere_multi_octets() {
        let texte = "é".repeat(5000);
        let parties = split_message(&texte);
        assert_eq!(parties.len(), 2, "5000 `é` → deux parties");
        let reconstruit: String = parties
            .iter()
            .enumerate()
            .map(|(rang, partie)| {
                partie
                    .strip_prefix(&format!("({}/{}) ", rang + 1, parties.len()))
                    .unwrap_or(partie)
                    .to_owned()
            })
            .collect();
        assert_eq!(
            reconstruit, texte,
            "la concaténation des corps retrouve les 5000 `é`"
        );
    }

    /// Scenario « troncature » : 100 000 caractères → `MAX_PARTS` parties, la dernière indique le
    /// nombre omis.
    #[test]
    fn troncature() {
        let paragraphe = "mot ".repeat(75); // 300 caractères
        let mut texte = String::new();
        while texte.chars().count() < 100_000 {
            texte.push_str(&paragraphe);
            texte.push('\n');
        }
        let parties = split_message(&texte);
        assert_eq!(parties.len(), MAX_PARTS, "la sortie est bornée à MAX_PARTS");
        let derniere = parties.last().unwrap_or_else(|| panic!("au moins une partie"));
        assert!(
            derniere.contains("sortie tronquée"),
            "la dernière partie annonce la troncature"
        );
        assert!(
            derniere.contains("caractères omis"),
            "elle cite le nombre omis : {derniere}"
        );
        assert!(
            derniere.chars().count() <= MAX_PART_CHARS,
            "dernière partie ≤ MAX_PART_CHARS"
        );
    }

    /// Scenario « `format_error` ne cite que le code » : `Échec du tour (code …).` — le code, et
    /// rien d'autre de variable.
    #[test]
    fn format_error_ne_cite_que_le_code() {
        let message = format_error("MRD-LYMAILLE-007");
        assert_eq!(message, "Échec du tour (code MRD-LYMAILLE-007).");
        let autre = format_error("MRD-LYMAILLE-009");
        assert_eq!(autre, "Échec du tour (code MRD-LYMAILLE-009).");
    }

    /// `Must` : `format_notice` — vide → `None` ; une ligne `Reprise : …` sinon.
    #[test]
    fn format_notice_en_une_ligne_ou_rien() {
        assert_eq!(format_notice(&[]), None, "aucun avis → None");
        let avis = format_notice(&["agent modifié".to_owned(), "toolset v2".to_owned()]);
        let attendu = avis.unwrap_or_else(|| panic!("des avis produisent une ligne"));
        assert!(
            attendu.starts_with("Reprise : "),
            "préfixe contractuel : {attendu}"
        );
        assert!(!attendu.contains('\n'), "une seule ligne : {attendu}");
        assert!(attendu.contains("agent modifié") && attendu.contains("toolset v2"));
    }

    /// `Must` : `help_text` couvre les commandes du pont (`!new <contexte> [agent]` après R13,
    /// `!cancel`, `!resume`, `!activate`, `!link`, `!help`).
    #[test]
    fn help_text_couvre_les_commandes() {
        let aide = help_text();
        for commande in ["!new", "!cancel", "!resume", "!activate", "!link", "!help"] {
            assert!(
                aide.contains(commande),
                "`{commande}` doit figurer dans l'aide : {aide}"
            );
        }
        assert!(
            aide.contains("contexte"),
            "`!new` prend un contexte depuis R13 : {aide}"
        );
    }

    /// Test de propriété (`Tasks` de `./render.sdd`) : la concaténation des parties retrouve le
    /// texte d'origine à la numérotation et aux marqueurs près — ici sans bloc de code, le
    /// décalage est exactement le préfixe retiré.
    #[test]
    fn propriete_concatenation_retrouve_le_texte() {
        for taille in [MAX_PART_CHARS + 1, 9000, 12345] {
            let mut texte = String::new();
            let mut compteur = 0u32;
            while texte.chars().count() < taille {
                compteur = compteur.wrapping_add(1);
                {
                    use std::fmt::Write as _;
                    let _ = write!(texte, "ligne {compteur} avec du texte et des accents éàü\n\n");
                }
            }
            texte = texte.chars().take(taille).collect();
            let parties = split_message(&texte);
            assert!(!parties.is_empty(), "texte non vide → au moins une partie");
            for (rang, partie) in parties.iter().enumerate() {
                assert!(!partie.is_empty(), "aucune partie vide");
                assert!(
                    partie.chars().count() <= MAX_PART_CHARS,
                    "partie {rang} de {} dépasse {MAX_PART_CHARS}",
                    partie.chars().count()
                );
            }
            if parties.len() > 1 && texte.chars().count() <= 20_000 {
                let reconstruit: String = parties
                    .iter()
                    .enumerate()
                    .map(|(rang, partie)| {
                        partie
                            .strip_prefix(&format!("({}/{}) ", rang + 1, parties.len()))
                            .unwrap_or(partie)
                            .to_owned()
                    })
                    .collect();
                assert_eq!(
                    reconstruit, texte,
                    "propriété de concaténation à {taille} caractères"
                );
            }
        }
    }
}
