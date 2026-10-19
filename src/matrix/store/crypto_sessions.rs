// Portions de ce fichier (schéma des tables, formats de sérialisation et sémantique des opérations) suivent
// `matrix-sdk-sqlite` 0.18.0, réimplémentés sur SeaORM.
// Copyright 2022, 2026 The Matrix.org Foundation C.I.C.
// Licensed under the Apache License, Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0).
// Modifié pour miryad-core : réécriture sur SeaORM (SQLite et PostgreSQL), autre découpage en fichiers.
// Le reste du fichier est sous la licence du dépôt (BSD-3-Clause), voir LICENSE et NOTICE.

//! Partie « sessions » du store de chiffrement : sessions Olm, sessions de groupe entrantes et
//! sortantes (Megolm), refus (`withheld`) et état de sauvegarde (`src/matrix/store/crypto_sessions.sdd`).
//! Fonctions inhérentes de `SeaOrmCryptoStore` ; l'`impl CryptoStore` les délègue
//! (`src/matrix/store/crypto.sdd`).

use std::sync::Arc;

use matrix_sdk::ruma::RoomId;
use matrix_sdk_crypto::olm::{
    Curve25519PublicKey, IdentityKeys, InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
    SenderDataType, Session,
};
use matrix_sdk_crypto::store::types::{Changes, RoomKeyCounts, RoomKeyWithheldEntry};
use sea_orm::sea_query::{Alias, Asterisk, Expr, ExprTrait, Func, OnConflict, Order, Query, SelectStatement};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbErr, QueryResult, StatementBuilder, TransactionTrait};

use super::crypto::SeaOrmCryptoStore;
use super::error::StoreError;

/// Tables du store, nom complet (`TABLE_PREFIX` + suffixe, `src/matrix/store/migration.sdd`).
const OLM_TABLE: &str = "miryad_matrix_olm_sessions";
const INBOUND_TABLE: &str = "miryad_matrix_inbound_group_sessions";
const OUTBOUND_TABLE: &str = "miryad_matrix_outbound_group_sessions";
const WITHHELD_TABLE: &str = "miryad_matrix_withheld_sessions";

/// Lit les lignes d'une requête (sur le store ou une transaction).
async fn query_rows(
    conn: &impl ConnectionTrait,
    select: &SelectStatement,
) -> Result<Vec<QueryResult>, StoreError> {
    let backend = conn.get_database_backend();
    Ok(conn
        .query_all_raw(StatementBuilder::build(select, &backend))
        .await?)
}

/// Exécute une écriture dans la transaction reçue.
async fn execute(txn: &DatabaseTransaction, stmt: &impl StatementBuilder) -> Result<(), StoreError> {
    let backend = txn.get_database_backend();
    txn.execute_raw(stmt.build(&backend)).await?;
    Ok(())
}

/// Insère ou remplace une ligne : conflit sur `primary_key`, les colonnes hors clé sont mises à jour.
async fn upsert(
    txn: &DatabaseTransaction,
    table: &str,
    columns: &[&str],
    values: Vec<Expr>,
    primary_key: &[&str],
) -> Result<(), StoreError> {
    let updated = columns
        .iter()
        .filter(|column| !primary_key.contains(column))
        .map(|column| Alias::new(*column));
    let stmt = Query::insert()
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|column| Alias::new(*column)))
        .values(values)
        .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
        .on_conflict(
            OnConflict::columns(primary_key.iter().map(|column| Alias::new(*column)))
                .update_columns(updated)
                .to_owned(),
        )
        .to_owned();
    execute(txn, &stmt).await
}

/// Convertit un décompte SQL en `usize`, refus si négatif.
fn count_from_row(row: Option<QueryResult>) -> Result<usize, StoreError> {
    let Some(row) = row else {
        return Ok(0);
    };
    let n: i64 = row.try_get("", "n")?;
    usize::try_from(n).map_err(|_| StoreError::Corrupt("compte négatif".into()))
}

impl SeaOrmCryptoStore {
    /// Hachage de recherche d'une valeur pour `table` (`Kv::hash`).
    fn hashed(&self, table: &str, value: &str) -> String {
        self.kv.hash(table, value)
    }

    /// Requête sur `table` restreinte au compte, projetant `columns`.
    fn scoped(&self, table: &str, columns: &[&str]) -> SelectStatement {
        let mut select = Query::select();
        for column in columns {
            select.column(Alias::new(*column));
        }
        select
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .to_owned()
    }

    /// Ouvre une session de groupe entrante depuis un payload scellé.
    fn inbound_from_sealed(&self, sealed: &[u8]) -> Result<InboundGroupSession, StoreError> {
        let pickle: PickledInboundGroupSession = self.kv.open_sealed(sealed)?;
        InboundGroupSession::from_pickle(pickle).map_err(|err| StoreError::Corrupt(err.to_string()))
    }

    /// Décode toutes les lignes `payload` d'une requête de sessions entrantes.
    fn inbound_from_rows(&self, rows: &[QueryResult]) -> Result<Vec<InboundGroupSession>, StoreError> {
        rows.iter()
            .map(|row| {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                self.inbound_from_sealed(&sealed)
            })
            .collect()
    }

    /// Sérialise une session de groupe entrante, drapeau `backed_up` de son pickle fixé à `backed_up`.
    async fn seal_inbound(
        &self,
        session: &InboundGroupSession,
        backed_up: bool,
    ) -> Result<Vec<u8>, StoreError> {
        let mut pickle = session.pickle().await;
        pickle.backed_up = backed_up;
        self.kv.seal(&pickle)
    }

    /// Écrit une session de groupe entrante dans `txn`, avec son état de sauvegarde.
    async fn upsert_inbound(
        &self,
        txn: &DatabaseTransaction,
        session: &InboundGroupSession,
        backed_up: bool,
    ) -> Result<(), StoreError> {
        let room_id = self.hashed(INBOUND_TABLE, session.room_id().as_str());
        let session_id = self.hashed(INBOUND_TABLE, session.session_id());
        let sender_key = self.hashed(INBOUND_TABLE, &session.sender_key().to_base64());
        let sender_data_type = session.sender_data_type() as i16;
        let sealed = self.seal_inbound(session, backed_up).await?;
        upsert(
            txn,
            INBOUND_TABLE,
            &[
                "account_key",
                "room_id",
                "session_id",
                "sender_key",
                "sender_data_type",
                "backed_up",
                "payload",
            ],
            vec![
                Expr::val(self.kv.account_key()),
                Expr::val(room_id),
                Expr::val(session_id),
                Expr::val(sender_key),
                Expr::val(sender_data_type),
                Expr::val(backed_up),
                Expr::val(sealed),
            ],
            &["account_key", "room_id", "session_id"],
        )
        .await
    }

    /// Met à jour le drapeau `backed_up` de la ligne `row` (colonne et pickle, re-scellé) dans `txn`.
    async fn set_backup_flag(
        &self,
        txn: &DatabaseTransaction,
        row: &QueryResult,
        backed_up: bool,
    ) -> Result<(), StoreError> {
        let room_id: String = row.try_get("", "room_id")?;
        let session_id: String = row.try_get("", "session_id")?;
        let sealed: Vec<u8> = row.try_get("", "payload")?;
        let mut pickle: PickledInboundGroupSession = self.kv.open_sealed(&sealed)?;
        pickle.backed_up = backed_up;
        let resealed = self.kv.seal(&pickle)?;
        let stmt = Query::update()
            .table(Alias::new(INBOUND_TABLE))
            .value(Alias::new("backed_up"), Expr::val(backed_up))
            .value(Alias::new("payload"), Expr::val(resealed))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .and_where(Expr::col(Alias::new("room_id")).eq(room_id))
            .and_where(Expr::col(Alias::new("session_id")).eq(session_id))
            .to_owned();
        execute(txn, &stmt).await
    }

    /// Décompte des sessions entrantes, éventuellement filtré sur `backed_up = true`.
    async fn count_inbound(&self, only_backed_up: bool) -> Result<usize, StoreError> {
        let mut select = Query::select();
        select
            .expr_as(Func::count(Expr::col(Asterisk)), Alias::new("n"))
            .from(Alias::new(INBOUND_TABLE))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()));
        if only_backed_up {
            select.and_where(Expr::col(Alias::new("backed_up")).eq(true));
        }
        count_from_row(query_rows(&self.db, &select).await?.into_iter().next())
    }

    /// Enregistre des sessions de groupe entrantes ; `backed_up_to_version` les marque sauvegardées.
    pub(super) async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<(), StoreError> {
        let backed_up = backed_up_to_version.is_some();
        let txn = self.db.begin().await?;
        for session in &sessions {
            self.upsert_inbound(&txn, session, backed_up).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Sessions Olm de l'expéditeur `sender_key` ; `None` si aucune.
    pub(super) async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>, StoreError> {
        let mut select = self.scoped(OLM_TABLE, &["payload"]);
        select.and_where(Expr::col(Alias::new("sender_key")).eq(self.hashed(OLM_TABLE, sender_key)));
        let rows = query_rows(&self.db, &select).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let account = self.load_account_for_unpickle().await?;
        let device_keys = account.device_keys();
        rows.iter()
            .map(|row| {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                let pickle = self.kv.open_sealed(&sealed)?;
                Session::from_pickle(device_keys.clone(), pickle)
                    .map_err(|err| StoreError::Corrupt(err.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// Session de groupe entrante `session_id` de `room_id`, si présente.
    pub(super) async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>, StoreError> {
        let mut select = self.scoped(INBOUND_TABLE, &["payload"]);
        select
            .and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(INBOUND_TABLE, room_id.as_str())))
            .and_where(Expr::col(Alias::new("session_id")).eq(self.hashed(INBOUND_TABLE, session_id)));
        let rows = query_rows(&self.db, &select).await?;
        Ok(self.inbound_from_rows(&rows)?.into_iter().next())
    }

    /// Refus enregistré pour `session_id` de `room_id`, s'il existe.
    pub(super) async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEntry>, StoreError> {
        let mut select = self.scoped(WITHHELD_TABLE, &["payload"]);
        select
            .and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(WITHHELD_TABLE, room_id.as_str())))
            .and_where(Expr::col(Alias::new("session_id")).eq(self.hashed(WITHHELD_TABLE, session_id)));
        let rows = query_rows(&self.db, &select).await?;
        match rows.first() {
            Some(row) => {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                Ok(Some(self.kv.open_sealed(&sealed)?))
            }
            None => Ok(None),
        }
    }

    /// Refus enregistrés pour `room_id`.
    pub(super) async fn get_withheld_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RoomKeyWithheldEntry>, StoreError> {
        let mut select = self.scoped(WITHHELD_TABLE, &["payload"]);
        select.and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(WITHHELD_TABLE, room_id.as_str())));
        query_rows(&self.db, &select)
            .await?
            .iter()
            .map(|row| {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                self.kv.open_sealed(&sealed)
            })
            .collect()
    }

    /// Toutes les sessions de groupe entrantes (seul chemin « tout lire », exigé par le trait).
    pub(super) async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>, StoreError> {
        let rows = query_rows(&self.db, &self.scoped(INBOUND_TABLE, &["payload"])).await?;
        self.inbound_from_rows(&rows)
    }

    /// Nombre total de sessions entrantes et nombre sauvegardées pour `backup_version`.
    /// Sans version, le nombre sauvegardé est zéro (`Handles` de `crypto_sessions.sdd`).
    pub(super) async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts, StoreError> {
        let total = self.count_inbound(false).await?;
        let backed_up = match backup_version {
            Some(_) => self.count_inbound(true).await?,
            None => 0,
        };
        Ok(RoomKeyCounts { total, backed_up })
    }

    /// Sessions entrantes de `room_id`.
    pub(super) async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        let mut select = self.scoped(INBOUND_TABLE, &["payload"]);
        select.and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(INBOUND_TABLE, room_id.as_str())));
        let rows = query_rows(&self.db, &select).await?;
        self.inbound_from_rows(&rows)
    }

    /// Lot de sessions entrantes de l'appareil `curve_key` et du type `sender_data_type`, après
    /// `after_session_id`, trié sur `session_id` haché (ordre déterministe, arbitrage de
    /// `crypto_sessions.sdd`).
    pub(super) async fn get_inbound_group_sessions_for_device_batch(
        &self,
        curve_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        let mut select = self.scoped(INBOUND_TABLE, &["payload"]);
        select
            .and_where(
                Expr::col(Alias::new("sender_key")).eq(self.hashed(INBOUND_TABLE, &curve_key.to_base64())),
            )
            .and_where(Expr::col(Alias::new("sender_data_type")).eq(sender_data_type as i16));
        if let Some(after) = after_session_id {
            select.and_where(Expr::col(Alias::new("session_id")).gt(self.hashed(INBOUND_TABLE, &after)));
        }
        select
            .order_by(Alias::new("session_id"), Order::Asc)
            .limit(u64::try_from(limit).unwrap_or(u64::MAX));
        let rows = query_rows(&self.db, &select).await?;
        self.inbound_from_rows(&rows)
    }

    /// Au plus `limit` sessions entrantes non sauvegardées. `backup_version` n'est pas filtré :
    /// le trait l'autorise (`traits.rs`), la colonne `backed_up` fait foi.
    pub(super) async fn inbound_group_sessions_for_backup(
        &self,
        backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        let _ = backup_version;
        let mut select = self.scoped(INBOUND_TABLE, &["payload"]);
        select
            .and_where(Expr::col(Alias::new("backed_up")).eq(false))
            .limit(u64::try_from(limit).unwrap_or(u64::MAX));
        let rows = query_rows(&self.db, &select).await?;
        self.inbound_from_rows(&rows)
    }

    /// Marque les sessions `(room, session)` comme sauvegardées (colonne et pickle).
    pub(super) async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<(), StoreError> {
        let _ = backup_version;
        let txn = self.db.begin().await?;
        for (room_id, session_id) in room_and_session_ids {
            let mut select = self.scoped(INBOUND_TABLE, &["room_id", "session_id", "payload"]);
            select
                .and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(INBOUND_TABLE, room_id.as_str())))
                .and_where(Expr::col(Alias::new("session_id")).eq(self.hashed(INBOUND_TABLE, session_id)));
            for row in query_rows(&txn, &select).await? {
                self.set_backup_flag(&txn, &row, true).await?;
            }
        }
        txn.commit().await?;
        Ok(())
    }

    /// Remet à non sauvegardées toutes les sessions entrantes (colonne et pickle).
    pub(super) async fn reset_backup_state(&self) -> Result<(), StoreError> {
        let txn = self.db.begin().await?;
        let select = self.scoped(INBOUND_TABLE, &["room_id", "session_id", "payload"]);
        for row in query_rows(&txn, &select).await? {
            self.set_backup_flag(&txn, &row, false).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Compte du store, requis pour réouvrir les sessions sortantes et Olm (clés de l'appareil).
    async fn load_account_for_unpickle(&self) -> Result<matrix_sdk_crypto::Account, StoreError> {
        self.load_account()
            .await?
            .ok_or_else(|| StoreError::Corrupt("pas de compte".into()))
    }

    /// Session de groupe sortante de `room_id`, si présente.
    pub(super) async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>, StoreError> {
        let mut select = self.scoped(OUTBOUND_TABLE, &["payload"]);
        select.and_where(Expr::col(Alias::new("room_id")).eq(self.hashed(OUTBOUND_TABLE, room_id.as_str())));
        let rows = query_rows(&self.db, &select).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let sealed: Vec<u8> = row.try_get("", "payload")?;
        let pickle = self.kv.open_sealed(&sealed)?;
        let account = self.load_account_for_unpickle().await?;
        let identity_keys: IdentityKeys = account.identity_keys();
        OutboundGroupSession::from_pickle(account.device_id().to_owned(), Arc::new(identity_keys), pickle)
            .map(Some)
            .map_err(|err| StoreError::Corrupt(err.to_string()))
    }

    /// Écrit les sessions Olm, entrantes, sortantes et refus de `changes` dans `txn`.
    pub(super) async fn apply_sessions(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        for session in &changes.sessions {
            let pickle = session.pickle().await;
            let sealed = self.kv.seal(&pickle)?;
            upsert(
                txn,
                OLM_TABLE,
                &["account_key", "session_id", "sender_key", "payload"],
                vec![
                    Expr::val(self.kv.account_key()),
                    Expr::val(self.hashed(OLM_TABLE, session.session_id())),
                    Expr::val(self.hashed(OLM_TABLE, &session.sender_key().to_base64())),
                    Expr::val(sealed),
                ],
                &["account_key", "session_id"],
            )
            .await?;
        }
        for session in &changes.inbound_group_sessions {
            self.upsert_inbound(txn, session, session.backed_up()).await?;
        }
        for session in &changes.outbound_group_sessions {
            let pickle = session.pickle().await;
            let sealed = self.kv.seal(&pickle)?;
            upsert(
                txn,
                OUTBOUND_TABLE,
                &["account_key", "room_id", "payload"],
                vec![
                    Expr::val(self.kv.account_key()),
                    Expr::val(self.hashed(OUTBOUND_TABLE, session.room_id().as_str())),
                    Expr::val(sealed),
                ],
                &["account_key", "room_id"],
            )
            .await?;
        }
        for (room_id, entries) in &changes.withheld_session_info {
            for (session_id, entry) in entries {
                let sealed = self.kv.seal(entry)?;
                upsert(
                    txn,
                    WITHHELD_TABLE,
                    &["account_key", "room_id", "session_id", "payload"],
                    vec![
                        Expr::val(self.kv.account_key()),
                        Expr::val(self.hashed(WITHHELD_TABLE, room_id.as_str())),
                        Expr::val(self.hashed(WITHHELD_TABLE, session_id)),
                        Expr::val(sealed),
                    ],
                    &["account_key", "room_id", "session_id"],
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::crypto::SeaOrmCryptoStore;
    use matrix_sdk::ruma::{OwnedUserId, RoomId, UserId};
    use matrix_sdk_crypto::olm::{EncryptionSettings, InboundGroupSession, SenderData, Session};
    use matrix_sdk_crypto::store::types::Changes;
    use matrix_sdk_crypto::{Account, DeviceData};
    use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
    use sea_orm_migration::MigratorTrait;

    const PASS: &str = "passphrase-1";
    const ROOM: &str = "!r:x";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    /// Store neuf sur SQLite mémoire migrée. Une seule connexion : sans elle, chaque
    /// connexion du pool verrait sa propre base mémoire.
    async fn fresh_store() -> SeaOrmCryptoStore {
        let db = super::super::test_support::test_db().await;
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        SeaOrmCryptoStore::open(db, "acc", PASS)
            .await
            .expect("store opens")
    }

    fn user(local: &str) -> OwnedUserId {
        UserId::parse(format!("@{local}:x")).expect("valid user id")
    }

    /// Une session de groupe entrante de `room`, créée avec l'API publique du compte.
    async fn inbound_session(account: &Account, room: &RoomId) -> InboundGroupSession {
        let (_outbound, inbound) = account
            .create_group_session_pair(room, EncryptionSettings::default(), SenderData::unknown())
            .await
            .expect("group session pair");
        inbound
    }

    /// Session Olm sortante de `ours` vers `theirs` (un one-time key frais par session).
    fn outbound_olm_session(ours: &Account, theirs: &mut Account) -> Session {
        theirs.generate_one_time_keys(1);
        let one_time_keys = theirs.signed_one_time_keys();
        let device = DeviceData::from_account(theirs);
        ours.create_outbound_session(&device, &one_time_keys, ours.device_keys())
            .expect("olm session towards the other account")
    }

    // Scenario: une session de groupe entrante fait l'aller-retour
    #[tokio::test]
    async fn inbound_group_session_round_trips() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let session = inbound_session(&account, room).await;
        let session_id = session.session_id().to_owned();
        let exported = serde_json::to_value(session.export().await.session_key)
            .expect("exported session key serializes");
        store
            .save_inbound_group_sessions(vec![session], None)
            .await
            .expect("save");
        let back = store
            .get_inbound_group_session(room, &session_id)
            .await
            .expect("get")
            .expect("the saved session is returned");
        assert_eq!(back.session_id(), session_id);
        assert_eq!(back.room_id(), room);
        let back_exported =
            serde_json::to_value(back.export().await.session_key).expect("exported session key serializes");
        assert_eq!(back_exported, exported, "même clé de session");
    }

    // Scenario: une session sauvegardée n'est plus proposée à la sauvegarde
    #[tokio::test]
    async fn backed_up_session_is_no_longer_offered_for_backup() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let first = inbound_session(&account, room).await;
        let second = inbound_session(&account, room).await;
        let id1 = first.session_id().to_owned();
        let id2 = second.session_id().to_owned();
        store
            .save_inbound_group_sessions(vec![first, second], None)
            .await
            .expect("save both sessions");
        store
            .mark_inbound_group_sessions_as_backed_up("v1", &[(room, id1.as_str())])
            .await
            .expect("mark the first session");
        let pending = store
            .inbound_group_sessions_for_backup("v1", 10)
            .await
            .expect("sessions for backup");
        let pending_ids: Vec<String> = pending.iter().map(|s| s.session_id().to_owned()).collect();
        assert_eq!(pending_ids, vec![id2], "seule la deuxième session est proposée");
        let counts = store
            .inbound_group_session_counts(Some("v1"))
            .await
            .expect("counts");
        assert_eq!((counts.total, counts.backed_up), (2, 1));
    }

    // Handles: sans version de sauvegarde, le total est rendu avec zéro sauvegardée
    #[tokio::test]
    async fn counts_without_backup_version_report_no_backed_up_session() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let session = inbound_session(&account, room).await;
        store
            .save_inbound_group_sessions(vec![session], Some("v1"))
            .await
            .expect("save as backed up");
        let counts = store.inbound_group_session_counts(None).await.expect("counts");
        assert_eq!((counts.total, counts.backed_up), (1, 0));
    }

    // Scenario: reset_backup_state remet tout à non sauvegardé
    #[tokio::test]
    async fn reset_backup_state_makes_sessions_offered_again() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let session = inbound_session(&account, room).await;
        let id = session.session_id().to_owned();
        store
            .save_inbound_group_sessions(vec![session], Some("v1"))
            .await
            .expect("save as backed up");
        store.reset_backup_state().await.expect("reset");
        let pending = store
            .inbound_group_sessions_for_backup("v1", 10)
            .await
            .expect("sessions for backup");
        let pending_ids: Vec<String> = pending.iter().map(|s| s.session_id().to_owned()).collect();
        assert_eq!(pending_ids, vec![id], "la session est de nouveau proposée");
    }

    // Handles (mark/reset) : le drapeau pickle-é relu suit la colonne, sans second passage
    #[tokio::test]
    async fn backup_flag_of_reloaded_session_follows_mark_and_reset() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let session = inbound_session(&account, room).await;
        let id = session.session_id().to_owned();
        store
            .save_inbound_group_sessions(vec![session], None)
            .await
            .expect("save");
        let flag = |s: Option<InboundGroupSession>| s.expect("session present").backed_up();
        store
            .mark_inbound_group_sessions_as_backed_up("v1", &[(room, id.as_str())])
            .await
            .expect("mark");
        let marked = store.get_inbound_group_session(room, &id).await.expect("get");
        assert!(flag(marked), "après mark, la session relue est sauvegardée");
        store.reset_backup_state().await.expect("reset");
        let reset = store.get_inbound_group_session(room, &id).await.expect("get");
        assert!(
            !flag(reset),
            "après reset, la session relue n'est plus sauvegardée"
        );
    }

    // Scenario: les sessions Olm sont retrouvées par clé d'expéditeur
    #[tokio::test]
    async fn olm_sessions_are_found_by_sender_key() {
        let store = fresh_store().await;
        let ours = Account::new(&user("alice"));
        let mut bob = Account::new(&user("bob"));
        let mut carol = Account::new(&user("carol"));
        let txn = store.db.begin().await.expect("begin");
        store.save_account(&txn, &ours).await.expect("save our account");
        txn.commit().await.expect("commit account");

        let first = outbound_olm_session(&ours, &mut bob);
        let second = outbound_olm_session(&ours, &mut bob);
        let third = outbound_olm_session(&ours, &mut carol);
        let mut expected = vec![first.session_id().to_owned(), second.session_id().to_owned()];
        expected.sort();
        let bob_key = bob.identity_keys().curve25519.to_base64();
        let carol_key = carol.identity_keys().curve25519.to_base64();
        let changes = Changes {
            sessions: vec![first, second, third],
            ..Default::default()
        };
        let txn = store.db.begin().await.expect("begin");
        store
            .apply_sessions(&txn, &changes)
            .await
            .expect("apply sessions");
        txn.commit().await.expect("commit sessions");

        let found = store
            .get_sessions(&bob_key)
            .await
            .expect("get_sessions for bob")
            .expect("two sessions for bob");
        let mut found_ids: Vec<String> = found.iter().map(|s| s.session_id().to_owned()).collect();
        found_ids.sort();
        assert_eq!(found_ids, expected, "exactement les deux sessions de bob");
        let for_carol = store
            .get_sessions(&carol_key)
            .await
            .expect("get_sessions for carol")
            .expect("one session for carol");
        assert_eq!(for_carol.len(), 1);
        let unknown = ours.identity_keys().curve25519.to_base64();
        assert!(
            store
                .get_sessions(&unknown)
                .await
                .expect("get_sessions for an unknown key")
                .is_none(),
            "clé inconnue : None"
        );
    }

    // Scenario: une colonne de recherche ne contient pas la valeur en clair
    #[tokio::test]
    async fn search_columns_and_payload_hide_room_and_session_ids() {
        let store = fresh_store().await;
        let account = Account::new(&user("alice"));
        let owned_room = RoomId::parse(ROOM).expect("valid room id");
        let room: &RoomId = &owned_room;
        let session = inbound_session(&account, room).await;
        let session_id = session.session_id().to_owned();
        store
            .save_inbound_group_sessions(vec![session], None)
            .await
            .expect("save");
        let rows = store
            .db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT account_key, room_id, session_id, sender_key, payload \
                 FROM miryad_matrix_inbound_group_sessions",
            ))
            .await
            .expect("raw select succeeds");
        assert_eq!(rows.len(), 1, "une ligne lue");
        for row in &rows {
            for column in ["account_key", "room_id", "session_id", "sender_key"] {
                let value: String = row.try_get("", column).expect("text column");
                assert!(
                    !value.contains(ROOM),
                    "{column} ne contient pas la salle en clair"
                );
                assert!(
                    !value.contains(&session_id),
                    "{column} ne contient pas le session_id"
                );
            }
            let payload: Vec<u8> = row.try_get("", "payload").expect("payload blob");
            assert!(
                !contains_bytes(&payload, ROOM.as_bytes()),
                "payload sans salle en clair"
            );
            assert!(
                !contains_bytes(&payload, session_id.as_bytes()),
                "payload sans session_id en clair"
            );
        }
    }

    /// Vrai si `needle` apparaît dans `haystack`.
    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }
}
