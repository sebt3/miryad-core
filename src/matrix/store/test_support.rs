//! Base de test du store, SQLite mémoire ou PostgreSQL réel (`src/matrix/store/test_support.sdd`).
//!
//! Compilé sous `#[cfg(test)]` seulement (déclaré par `mod.rs`).

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};

/// Variable d'environnement qui porte l'URL PostgreSQL de test (jamais affichée).
const PG_URL_VAR: &str = "MIRYAD_TEST_PG_URL";

/// URL PostgreSQL de test, si la variable est posée et non vide.
fn pg_url() -> Option<String> {
    std::env::var(PG_URL_VAR).ok().filter(|v| !v.is_empty())
}

/// Vrai si `MIRYAD_TEST_PG_URL` est posée et non vide.
pub(crate) fn is_postgres() -> bool {
    pg_url().is_some()
}

/// Base neuve pour un test, non migrée (l'appelant applique ses migrations).
///
/// Sans `MIRYAD_TEST_PG_URL` : SQLite mémoire, une seule connexion (sinon chaque connexion
/// du pool verrait sa propre base). Avec : un schéma neuf `t_<hex>` sur la base PostgreSQL,
/// et un pool dont le `search_path` est ce schéma.
///
/// Les messages d'échec ne contiennent jamais l'URL (elle porte un mot de passe) : l'erreur
/// `DbErr` d'origine est donc écartée, pas affichée.
pub(crate) async fn test_db() -> DatabaseConnection {
    match pg_url() {
        None => {
            let mut opts = ConnectOptions::new("sqlite::memory:");
            opts.max_connections(1).min_connections(1);
            Database::connect(opts)
                .await
                .unwrap_or_else(|_| panic!("base SQLite mémoire de test : connexion impossible"))
        }
        Some(url) => pg_schema_db(url).await,
    }
}

/// Crée un schéma `t_<16 hex>` via une connexion d'amorçage, puis rend un pool dont le
/// `search_path` est ce schéma. Les schémas créés restent : la base de test est jetable.
async fn pg_schema_db(url: String) -> DatabaseConnection {
    // L'erreur d'origine est conservée pour le diagnostic, l'URL (mot de passe) en est retirée.
    let scrub = |what: &str, err: sea_orm::DbErr| -> String {
        format!(
            "PostgreSQL de test : {what} : {}",
            err.to_string().replace(url.as_str(), "<url>")
        )
    };
    let schema = format!("t_{:016x}", rand::random::<u64>());
    let boot = Database::connect(url.as_str())
        .await
        .unwrap_or_else(|err| panic!("{}", scrub("connexion d'amorçage impossible", err)));
    boot.execute_unprepared(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .unwrap_or_else(|err| panic!("{}", scrub("création du schéma de test impossible", err)));
    boot.close()
        .await
        .unwrap_or_else(|err| panic!("{}", scrub("fermeture de l'amorçage impossible", err)));
    let mut opts = ConnectOptions::new(url.clone());
    opts.set_schema_search_path(schema);
    Database::connect(opts)
        .await
        .unwrap_or_else(|err| panic!("{}", scrub("pool sur le schéma de test impossible", err)))
}

#[cfg(test)]
mod tests {
    use super::{is_postgres, test_db};
    use sea_orm::{ConnectionTrait, Statement};

    /// Vrai quand la suite tourne sur PostgreSQL : les scénarios SQLite ne s'y appliquent pas.
    fn postgres_requested() -> bool {
        std::env::var("MIRYAD_TEST_PG_URL").is_ok_and(|v| !v.is_empty())
    }

    /// Scenario : sans variable, le helper rend une base SQLite mémoire utilisable.
    #[tokio::test]
    async fn sans_variable_le_helper_rend_une_base_sqlite_memoire_utilisable() {
        if postgres_requested() {
            return;
        }
        let db = test_db().await;
        let row = db
            .query_one_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT 1 AS n".to_owned(),
            ))
            .await;
        assert!(row.is_ok(), "SELECT 1 doit réussir sur la base de test");
        assert!(
            !is_postgres(),
            "sans MIRYAD_TEST_PG_URL, is_postgres() doit valoir false"
        );
    }

    /// Scenario : deux appels rendent deux bases isolées.
    #[tokio::test]
    async fn deux_appels_rendent_deux_bases_isolees() {
        if postgres_requested() {
            return;
        }
        let first = test_db().await;
        let second = test_db().await;
        first
            .execute_unprepared("CREATE TABLE probe_isolation (x INTEGER)")
            .await
            .expect("create on first base");
        let found = second
            .query_one_raw(Statement::from_string(
                second.get_database_backend(),
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'probe_isolation'".to_owned(),
            ))
            .await
            .expect("query on second base");
        assert!(
            found.is_none(),
            "la table créée sur la première base ne doit pas exister sur la seconde"
        );
    }
}
