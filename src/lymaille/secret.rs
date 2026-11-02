//! Résolution des références d'authentification des providers (`src/lymaille/secret.sdd`).
//!
//! Un `Provider` de `lymaille` ne porte que des références (`auth: Option<String>`), jamais
//! de secret (DSD `DEC-16`). La valeur n'est résolue qu'à la construction du client
//! d'inférence, vit dans un `Zeroizing<String>` et ne traverse ni erreur, ni log, ni session,
//! ni base.

use zeroize::Zeroizing;

use super::error::LymailleError;

/// Trait par lequel l'application consommatrice transforme une référence de secret en clé
/// d'API au moment de construire le client d'inférence. Synchrone : une variable
/// d'environnement ou un fichier ne justifient pas un `async`. S'utilise par
/// `Arc<dyn SecretResolver>` fourni au montage de l'état.
pub trait SecretResolver: Send + Sync {
    /// Résout `reference` en secret. La valeur vit dans un `Zeroizing<String>` et ne
    /// transite par aucune erreur, aucun log, aucune session.
    ///
    /// # Errors
    ///
    /// `LymailleError::SecretUnresolved` (`MRD-LYMAILLE-005`) si la référence est introuvable
    /// ou vide ; le message porte la référence, jamais la valeur.
    fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, LymailleError>;
}

/// `SecretResolver` par défaut : la référence est un nom de variable d'environnement. Le
/// `Debug` dérivé d'un struct unitaire n'imprime aucune valeur d'environnement.
#[derive(Debug, Default, Clone)]
pub struct EnvSecretResolver;

impl SecretResolver for EnvSecretResolver {
    fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, LymailleError> {
        match std::env::var(reference) {
            Ok(value) if !value.is_empty() => Ok(Zeroizing::new(value)),
            // Absente (`NotPresent`), vide ou non UTF-8 (`NotUnicode`) : la référence est
            // rendue, jamais la valeur.
            _ => Err(LymailleError::SecretUnresolved(reference.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Output};
    use std::sync::Arc;

    const KEY: &str = "MRD_TEST_LYM_KEY";
    const SECRET: &str = "s3cret";
    const MODE: &str = "MRD_TEST_LYM_MODE";
    const UNKNOWN: &str = "MRD_TEST_LYM_INCONNUE";

    // Pourquoi ce montage : le harnais interdit `unsafe_code` (`forbid`, y compris sous
    // `cfg(test)`) et l'édition 2024 rend `std::env::set_var` / `remove_var` `unsafe` : muter
    // l'environnement du processus depuis un test est impossible sans enfreindre le harnais.
    // Chaque test se redéroule donc lui-même dans un processus fils lancé avec l'environnement
    // voulu (`Command::env`, sûr car propre au fils), filtré sur lui-même par le nom de test.
    // Conséquence : aucun test ne mute l'environnement du processus courant — aucune course
    // entre tests parallèles, et l'état global (provider TLS, variables) des autres tests
    // n'est jamais écrasé.

    #[derive(Clone, Copy)]
    enum KeyEnv<'a> {
        Set(&'a str),
        Remove,
    }

    fn rerun(test_name: &str, mode: &str, key: KeyEnv<'_>) -> Output {
        let mut command = Command::new(std::env::current_exe().expect("binaire de test introuvable"));
        command.arg(test_name).env(MODE, mode);
        match key {
            KeyEnv::Set(value) => {
                command.env(KEY, value);
            }
            KeyEnv::Remove => {
                command.env_remove(KEY);
            }
        }
        command.output().expect("relance du binaire de test impossible")
    }

    fn assert_child_ok(output: &Output, what: &str) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed"),
            "{what} : fils en échec, statut {:?}, stdout :\n{stdout}\nstderr :\n{stderr}",
            output.status.code(),
        );
    }

    fn assert_secret_unresolved(error: &LymailleError, reference: &str) {
        match error {
            LymailleError::SecretUnresolved(found) => assert_eq!(found, reference),
            other => panic!("attendu SecretUnresolved({reference:?}), obtenu : {other:?}"),
        }
        assert!(
            error.to_string().contains("MRD-LYMAILLE-005"),
            "code de l'erreur manquant : {error}"
        );
    }

    // Scenario: la variable présente est résolue
    #[test]
    fn present_variable_is_resolved() {
        match std::env::var(MODE).ok().as_deref() {
            Some("present") => {
                assert_eq!(std::env::var(KEY).expect("prérecondition du Scenario"), SECRET);
                let resolved = EnvSecretResolver
                    .resolve(KEY)
                    .expect("la variable présente doit être résolue");
                assert_eq!(&**resolved, SECRET);
                // Must : le trait s'utilise par `Arc<dyn SecretResolver>` (objet-trait, Send + Sync).
                let dyn_resolver: Arc<dyn SecretResolver> = Arc::new(EnvSecretResolver);
                assert_eq!(
                    &**dyn_resolver.resolve(KEY).expect("résolu via l'objet-trait"),
                    SECRET
                );
            }
            None => assert_child_ok(
                &rerun("present_variable_is_resolved", "present", KeyEnv::Set(SECRET)),
                "variable présente",
            ),
            Some(other) => unreachable!("mode fils inattendu : {other}"),
        }
    }

    // Scenario: une référence absente ou vide est une erreur sans valeur
    #[test]
    fn absent_or_empty_reference_is_error() {
        match std::env::var(MODE).ok().as_deref() {
            Some("absent") => {
                assert!(
                    std::env::var(KEY).is_err(),
                    "prérecondition : la variable doit être absente"
                );
                let error = EnvSecretResolver
                    .resolve(KEY)
                    .expect_err("variable absente -> erreur");
                assert_secret_unresolved(&error, KEY);
            }
            Some("vide") => {
                assert_eq!(
                    std::env::var(KEY).expect("prérecondition : la variable vide doit être présente"),
                    ""
                );
                let error = EnvSecretResolver
                    .resolve(KEY)
                    .expect_err("variable vide -> erreur");
                assert_secret_unresolved(&error, KEY);
            }
            None => {
                assert_child_ok(
                    &rerun("absent_or_empty_reference_is_error", "absent", KeyEnv::Remove),
                    "variable absente",
                );
                assert_child_ok(
                    &rerun("absent_or_empty_reference_is_error", "vide", KeyEnv::Set("")),
                    "variable vide",
                );
            }
            Some(other) => unreachable!("mode fils inattendu : {other}"),
        }
    }

    // Scenario: aucune valeur secrète dans les formats
    #[test]
    fn no_secret_value_in_formats() {
        match std::env::var(MODE).ok().as_deref() {
            Some("formats") => {
                assert_eq!(std::env::var(KEY).expect("prérecondition du Scenario"), SECRET);
                let error = EnvSecretResolver
                    .resolve(UNKNOWN)
                    .expect_err("référence inconnue -> erreur");
                for text in [
                    format!("{EnvSecretResolver:?}"),
                    format!("{error:?}"),
                    error.to_string(),
                ] {
                    assert!(
                        !text.contains(SECRET),
                        "valeur secrète aperçue dans un format : {text}"
                    );
                }
                assert!(
                    error.to_string().contains(UNKNOWN),
                    "la référence doit rester visible dans l'erreur : {error}"
                );
            }
            None => assert_child_ok(
                &rerun("no_secret_value_in_formats", "formats", KeyEnv::Set(SECRET)),
                "formats sans secret",
            ),
            Some(other) => unreachable!("mode fils inattendu : {other}"),
        }
    }

    // Must (`secret.sdd`) : une variable non UTF-8 est une `SecretUnresolved` sur la
    // référence. Montage fils identique (`Command::env` accepte un `OsString` non UTF-8).
    #[cfg(unix)]
    #[test]
    fn non_utf8_variable_is_error() {
        use std::os::unix::ffi::OsStrExt as _;
        match std::env::var(MODE).ok().as_deref() {
            Some("non_utf8") => {
                assert!(
                    std::env::var(KEY).is_err(),
                    "prérecondition : l'octet 0xff n'est pas du UTF-8 valide"
                );
                assert!(
                    std::env::var_os(KEY).is_some_and(|raw| raw.as_bytes() == [0xff].as_slice()),
                    "prérecondition : KEY doit porter l'octet 0xff"
                );
                let error = EnvSecretResolver
                    .resolve(KEY)
                    .expect_err("variable non UTF-8 -> erreur");
                assert_secret_unresolved(&error, KEY);
            }
            None => {
                let mut command = Command::new(std::env::current_exe().expect("binaire de test introuvable"));
                command
                    .arg("non_utf8_variable_is_error")
                    .env(MODE, "non_utf8")
                    .env(KEY, std::ffi::OsStr::from_bytes(&[0xff]));
                assert_child_ok(
                    &command.output().expect("relance du binaire de test impossible"),
                    "variable non UTF-8",
                );
            }
            Some(other) => unreachable!("mode fils inattendu : {other}"),
        }
    }
}
