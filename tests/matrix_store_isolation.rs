//! Crate d'intégration rattachée à `src/matrix/store/mod.sdd` (`Can modify`) : garde-fou
//! d'extractibilité du store de chiffrement. Le test lit les sources de `src/matrix/store/*.rs` et
//! échoue si un chemin `crate::` pointe hors de `crate::matrix::store`, si un chemin `super::`
//! remonte hors du module, ou si le nom propre de la crate (`miryad_core::`) apparaît. Le
//! `Scenario` « aucun import hors du module » ; le `Scenario` « les ré-exports sont exactement ceux
//! de Must » est exercé depuis l'extérieur de la crate (`miryad_core::matrix::store::…`).

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

use std::path::Path;

/// Racine des sources du store, relative au manifeste.
const STORE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/matrix/store");

/// Préfixe de chemin autorisé pour `crate::`.
const ALLOWED_CRATE_PREFIX: &str = "crate::matrix::store";

/// Détecte les violations d'isolation d'un fichier source du store.
///
/// `file` est le nom de fichier (`mod.rs`, `kv.rs`…), qui fixe la profondeur de module :
/// `mod.rs` est `store`, un enfant est `store::<nom>`, et le bloc `mod tests` ajoute un niveau.
/// Un `super::` remonte d'un niveau ; une chaîne qui sort de `store` est une violation.
fn violations(file: &str, source: &str) -> Vec<String> {
    let base_depth: usize = usize::from(file != "mod.rs");
    let mut in_tests = false;
    let mut found = Vec::new();
    for line in source.lines() {
        if line.starts_with("mod tests") {
            in_tests = true;
        }
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let allowed = base_depth + usize::from(in_tests);
        if code.contains("miryad_core::") {
            found.push(format!("{file}: chemin absolu de crate : {line}"));
        }
        for (position, _) in code.match_indices("crate::") {
            if !is_path_start(code, position) {
                continue;
            }
            let path = code.get(position..).unwrap_or_default();
            if !path.starts_with(ALLOWED_CRATE_PREFIX) {
                found.push(format!("{file}: crate:: hors de {ALLOWED_CRATE_PREFIX} : {line}"));
            }
        }
        for (position, _) in code.match_indices("super::") {
            if !is_path_start(code, position) {
                continue;
            }
            let chain = code.get(position..).unwrap_or_default();
            let consecutive = count_consecutive_supers(chain);
            if consecutive > allowed {
                found.push(format!(
                    "{file}: super:: remonte de {consecutive} niveau(x), {allowed} permis : {line}"
                ));
            }
        }
    }
    found
}

/// Vrai si la position est un début de chemin (pas le milieu d'un identifiant ni d'un `::`).
fn is_path_start(code: &str, position: usize) -> bool {
    match code.get(..position).and_then(|head| head.chars().last()) {
        None => true,
        Some(previous) => !(previous.is_alphanumeric() || previous == '_' || previous == ':'),
    }
}

/// Nombre de `super::` consécutifs au début de `chain` (`super::super::x` → 2).
fn count_consecutive_supers(chain: &str) -> usize {
    let mut rest = chain;
    let mut count = 0;
    while let Some(tail) = rest.strip_prefix("super::") {
        count += 1;
        rest = tail;
    }
    count
}

/// Scenario « aucun import hors du module » : les sources réelles du store ne sortent pas de
/// `crate::matrix::store`, et aucun `super::` ne remonte au-delà de `store`.
#[test]
fn aucun_import_hors_du_module() {
    let mut scanned = 0_usize;
    let mut found = Vec::new();
    let entries = std::fs::read_dir(Path::new(STORE_DIR)).expect("lecture de src/matrix/store");
    for entry in entries {
        let entry = entry.expect("entrée de src/matrix/store");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).expect("lecture d'un source du store");
        scanned += 1;
        found.extend(violations(&name, &source));
    }
    assert!(scanned >= 8, "moins de 8 sources scannés : {scanned}");
    assert!(
        found.is_empty(),
        "imports hors de crate::matrix::store :\n{}",
        found.join("\n")
    );
}

/// Test négatif : le détecteur doit attraper une violation fabriquée, sans toucher aux sources.
#[test]
fn le_detecteur_attrape_une_violation_fabriquee() {
    let crate_import = violations("kv.rs", "use crate::resource::MiryadResource;\n");
    assert_eq!(
        crate_import.len(),
        1,
        "crate:: hors du module non détecté : {crate_import:?}"
    );

    let escape = violations("crypto.rs", "use super::super::super::auth::X;\n");
    assert_eq!(escape.len(), 1, "super::super::super non détecté : {escape:?}");

    let escape_top = violations("mod.rs", "use super::super::workflow::Y;\n");
    assert_eq!(
        escape_top.len(),
        1,
        "super depuis mod.rs non détecté : {escape_top:?}"
    );

    let absolute = violations("kv.rs", "let _ = miryad_core::resource::X;\n");
    assert_eq!(
        absolute.len(),
        1,
        "chemin absolu miryad_core:: non détecté : {absolute:?}"
    );
}

/// Test positif du détecteur : les formes légitimes ne sont pas signalées.
#[test]
fn le_detecteur_laisse_passer_les_formes_legitimes() {
    let own = violations("kv.rs", "use crate::matrix::store::error::StoreError;\n");
    assert!(own.is_empty(), "crate::matrix::store signalé à tort : {own:?}");

    let sibling = violations("crypto.rs", "use super::error::StoreError;\n");
    assert!(sibling.is_empty(), "super:: frère signalé à tort : {sibling:?}");

    let tests_block = violations(
        "kv.rs",
        "mod tests {\n    use super::super::migration::migrations;\n}\n",
    );
    assert!(
        tests_block.is_empty(),
        "super::super dans mod tests signalé à tort : {tests_block:?}"
    );

    let comment = violations("kv.rs", "// exemple : use crate::resource::X;\n");
    assert!(comment.is_empty(), "commentaire signalé à tort : {comment:?}");
}

/// Scenario « les ré-exports sont exactement ceux de Must » : chemins posés depuis l'extérieur de
/// la crate, `TABLE_PREFIX` valant `miryad_matrix_`.
#[cfg(feature = "matrix")]
mod reexports {
    use miryad_core::matrix::store::{SeaOrmCryptoStore, StoreError, TABLE_PREFIX, migrations};

    #[test]
    fn les_reexports_sont_exactement_ceux_de_must() {
        fn witness(_: Option<SeaOrmCryptoStore>, _: Option<StoreError>) {}
        witness(None, None);
        let _: fn() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> = migrations;
        assert_eq!(TABLE_PREFIX, "miryad_matrix_");
    }
}
