//! Primitives communes du store de chiffrement : `StoreCipher` du compte, scellement des payloads,
//! hachage des colonnes de recherche, accès clé/valeur à la table `kv` (`src/matrix/store/kv.sdd`).

use std::sync::Arc;

use matrix_sdk_store_encryption::{Error as CipherError, StoreCipher};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, OnConflict, Query};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, StatementBuilder};
use serde::{Serialize, de::DeserializeOwned};

use super::error::StoreError;

/// Table `kv` complète (`TABLE_PREFIX` + `kv`, `src/matrix/store/migration.sdd`).
const TABLE: &str = "miryad_matrix_kv";

/// Clé de la ligne qui porte le cipher exporté du compte.
const CIPHER_KEY: &str = "cipher";

/// Accès aux primitives du store pour un compte (`account_key`).
#[derive(Clone)]
pub(super) struct Kv {
    db: DatabaseConnection,
    account_key: String,
    cipher: Arc<StoreCipher>,
}

impl std::fmt::Debug for Kv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kv").finish_non_exhaustive()
    }
}

impl Kv {
    /// Ouvre le store du compte : crée le cipher au premier appel, le ré-importe ensuite.
    pub(super) async fn open(
        db: DatabaseConnection,
        account_key: &str,
        passphrase: &str,
    ) -> Result<Kv, StoreError> {
        let cipher = if let Some(exported) = select_value(&db, account_key, CIPHER_KEY).await? {
            import_cipher(passphrase, &exported)?
        } else {
            let fresh = StoreCipher::new().map_err(|err| cipher_error(&err))?;
            let exported = fresh.export(passphrase).map_err(|err| cipher_error(&err))?;
            insert_row(&db, account_key, CIPHER_KEY, exported, insert_if_absent()).await?;
            // Relit la ligne gagnante : en concurrence, un autre `open` a pu écrire avant nous.
            let winner = select_value(&db, account_key, CIPHER_KEY)
                .await?
                .ok_or_else(|| StoreError::Corrupt("ligne cipher absente après insertion".into()))?;
            import_cipher(passphrase, &winner)?
        };
        Ok(Kv {
            db,
            account_key: account_key.to_owned(),
            cipher: Arc::new(cipher),
        })
    }

    /// Clé de compte (`account_key`) de ce store : filtre des tables partagées (ex. `leases`).
    pub(super) fn account_key(&self) -> &str {
        &self.account_key
    }

    /// Hachage hexadécimal déterministe de `value` dans `table`.
    pub(super) fn hash(&self, table: &str, value: &str) -> String {
        hex::encode(self.cipher.hash_key(table, value.as_bytes()))
    }

    /// Sérialise en JSON puis chiffre.
    pub(super) fn seal<T: Serialize>(&self, v: &T) -> Result<Vec<u8>, StoreError> {
        self.cipher.encrypt_value(v).map_err(|err| match err {
            CipherError::Json(_) | CipherError::Serialization(_) => {
                StoreError::Serialization(err.to_string())
            }
            _ => StoreError::Cipher(err.to_string()),
        })
    }

    /// Déchiffre puis désérialise.
    pub(super) fn open_sealed<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, StoreError> {
        self.cipher
            .decrypt_value::<T>(bytes)
            .map_err(|err| StoreError::Corrupt(err.to_string()))
    }

    /// Lit la valeur scellée de `key` pour ce compte.
    pub(super) async fn get(
        &self,
        conn: &impl ConnectionTrait,
        key: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        select_value(conn, &self.account_key, key).await
    }

    /// Insère ou remplace la valeur scellée de `key` pour ce compte.
    pub(super) async fn put(
        &self,
        conn: &impl ConnectionTrait,
        key: &str,
        value: Vec<u8>,
    ) -> Result<(), StoreError> {
        insert_row(conn, &self.account_key, key, value, upsert()).await
    }

    /// Supprime la ligne `key` de ce compte.
    pub(super) async fn delete(&self, conn: &impl ConnectionTrait, key: &str) -> Result<(), StoreError> {
        let backend = conn.get_database_backend();
        let stmt = Query::delete()
            .from_table(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.account_key.as_str()))
            .and_where(Expr::col(Alias::new("key")).eq(key))
            .to_owned();
        conn.execute_raw(StatementBuilder::build(&stmt, &backend)).await?;
        Ok(())
    }
}

/// Erreur de chiffrement du store, sans contenu de payload ni de passphrase.
fn cipher_error(err: &CipherError) -> StoreError {
    StoreError::Cipher(err.to_string())
}

/// Ré-importe un cipher exporté avec la passphrase du compte.
fn import_cipher(passphrase: &str, exported: &[u8]) -> Result<StoreCipher, StoreError> {
    StoreCipher::import(passphrase, exported).map_err(|err| cipher_error(&err))
}

/// Clause de conflit « ne rien faire » : insertion conditionnelle.
fn insert_if_absent() -> OnConflict {
    OnConflict::columns([Alias::new("account_key"), Alias::new("key")])
        .do_nothing()
        .to_owned()
}

/// Clause de conflit « remplacer la valeur » : upsert.
fn upsert() -> OnConflict {
    OnConflict::columns([Alias::new("account_key"), Alias::new("key")])
        .update_column(Alias::new("value"))
        .to_owned()
}

/// Lit la valeur de `key` pour `account_key`, si la ligne existe.
async fn select_value(
    conn: &impl ConnectionTrait,
    account_key: &str,
    key: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    let backend = conn.get_database_backend();
    let stmt = Query::select()
        .column(Alias::new("value"))
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("account_key")).eq(account_key))
        .and_where(Expr::col(Alias::new("key")).eq(key))
        .to_owned();
    let row = conn
        .query_one_raw(StatementBuilder::build(&stmt, &backend))
        .await?;
    Ok(match row {
        Some(row) => Some(row.try_get("", "value")?),
        None => None,
    })
}

/// Insère une ligne `(account_key, key, value)` ; `conflict` décide du cas de clé existante.
async fn insert_row(
    conn: &impl ConnectionTrait,
    account_key: &str,
    key: &str,
    value: Vec<u8>,
    conflict: OnConflict,
) -> Result<(), StoreError> {
    let backend = conn.get_database_backend();
    let stmt = Query::insert()
        .into_table(Alias::new(TABLE))
        .columns([Alias::new("account_key"), Alias::new("key"), Alias::new("value")])
        .values([Expr::val(account_key), Expr::val(key), Expr::val(value)])
        .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
        .on_conflict(conflict)
        .to_owned();
    conn.execute_raw(StatementBuilder::build(&stmt, &backend)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Kv, StoreError};
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    const ACC: &str = "acc-1";
    const PASS: &str = "passphrase-1";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    /// Base SQLite mémoire migrée (tables du store posées).
    async fn fresh_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        db
    }

    /// Nombre de lignes `cipher` du compte.
    async fn cipher_rows(db: &DatabaseConnection, account_key: &str) -> i64 {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS n FROM miryad_matrix_kv WHERE account_key = ? AND key = 'cipher'",
                [account_key.into()],
            ))
            .await
            .expect("count query succeeds")
            .expect("COUNT returns one row");
        row.try_get("", "n").expect("column n is readable")
    }

    // Scenario: le premier open crée un cipher, le second le relit
    #[tokio::test]
    async fn first_open_creates_cipher_and_second_open_reads_it() {
        let db = fresh_db().await;
        let first = Kv::open(db.clone(), ACC, PASS)
            .await
            .expect("first open creates the cipher");
        let second = Kv::open(db.clone(), ACC, PASS)
            .await
            .expect("second open re-imports the cipher");
        assert_eq!(cipher_rows(&db, ACC).await, 1, "une seule ligne cipher");
        let sealed = first
            .seal(&"secret".to_owned())
            .expect("seal with the first instance");
        let read: String = second
            .open_sealed(&sealed)
            .expect("second instance opens a payload sealed by the first");
        assert_eq!(read, "secret");
    }

    // Scenario: une mauvaise passphrase est une erreur, pas un cipher neuf
    #[tokio::test]
    async fn wrong_passphrase_is_an_error_and_keeps_cipher_row() {
        let db = fresh_db().await;
        let kv = Kv::open(db.clone(), ACC, "a")
            .await
            .expect("open with passphrase a");
        let before = kv
            .get(&db, "cipher")
            .await
            .expect("cipher row readable")
            .expect("cipher row exists");
        match Kv::open(db.clone(), ACC, "b").await {
            Err(StoreError::Cipher(_)) => {}
            Err(other) => panic!("attendu StoreError::Cipher, obtenu : {other}"),
            Ok(_) => panic!("une mauvaise passphrase ne doit pas ouvrir le store"),
        }
        let after = kv
            .get(&db, "cipher")
            .await
            .expect("cipher row readable")
            .expect("cipher row still exists");
        assert_eq!(before, after, "la ligne cipher est inchangée");
    }

    // Scenario: le hachage est déterministe et dépend de la table
    #[tokio::test]
    async fn hash_is_deterministic_and_depends_on_table() {
        let db = fresh_db().await;
        let kv = Kv::open(db, ACC, PASS).await.expect("open");
        let h1 = kv.hash("t1", "x");
        let h2 = kv.hash("t1", "x");
        let h3 = kv.hash("t2", "x");
        assert_eq!(h1, h2, "même (table, valeur) -> même hachage");
        assert_ne!(h1, h3, "la table entre dans le hachage");
    }

    // Scenario: put est un upsert, delete supprime
    #[tokio::test]
    async fn put_upserts_and_delete_removes() {
        let db = fresh_db().await;
        let kv = Kv::open(db.clone(), ACC, PASS).await.expect("open");
        assert_eq!(kv.get(&db, "k").await.expect("get"), None, "clé absente");
        kv.put(&db, "k", vec![1]).await.expect("first put");
        kv.put(&db, "k", vec![2, 3]).await.expect("upsert put");
        assert_eq!(
            kv.get(&db, "k").await.expect("get"),
            Some(vec![2, 3]),
            "la seconde valeur"
        );
        kv.delete(&db, "k").await.expect("delete");
        assert_eq!(kv.get(&db, "k").await.expect("get"), None, "supprimée");
    }

    // Scenario: deux comptes sont isolés (+ Handles : une passphrase n'ouvre pas l'autre compte)
    #[tokio::test]
    async fn two_accounts_are_isolated() {
        let db = fresh_db().await;
        let a = Kv::open(db.clone(), "acc-a", "pa").await.expect("open a");
        let b = Kv::open(db.clone(), "acc-b", "pb").await.expect("open b");
        a.put(&db, "k", b"from-a".to_vec()).await.expect("put a");
        b.put(&db, "k", b"from-b".to_vec()).await.expect("put b");
        assert_eq!(a.get(&db, "k").await.expect("get a"), Some(b"from-a".to_vec()));
        assert_eq!(b.get(&db, "k").await.expect("get b"), Some(b"from-b".to_vec()));
        assert!(
            matches!(
                Kv::open(db.clone(), "acc-b", "pa").await,
                Err(StoreError::Cipher(_))
            ),
            "la passphrase de a n'ouvre pas b"
        );
    }
}
