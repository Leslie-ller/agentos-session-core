use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE conversation_handoff (
                conversation_id INTEGER PRIMARY KEY REFERENCES conversation(id) ON DELETE CASCADE,
                revision INTEGER NOT NULL,
                archive_json TEXT NOT NULL,
                retired_sessions_json TEXT NOT NULL,
                context_prefix TEXT NOT NULL
            )",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE conversation_handoff")
            .await?;
        Ok(())
    }
}
