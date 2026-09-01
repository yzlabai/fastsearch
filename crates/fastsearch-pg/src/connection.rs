use crate::Result;
use tokio::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

/// One serialized PostgreSQL client that replaces a closed connection in place.
///
/// Callers only learn one interface: acquire a usable client. Driver lifecycle and reconnect
/// policy stay local, so source-of-truth and ingestion stores cannot drift independently.
pub(crate) struct ReconnectingClient {
    client: Mutex<Client>,
    database_url: String,
    label: &'static str,
}

impl ReconnectingClient {
    pub(crate) async fn connect(database_url: &str, label: &'static str) -> Result<Self> {
        Ok(Self {
            client: Mutex::new(Self::open(database_url, label).await?),
            database_url: database_url.to_string(),
            label,
        })
    }

    pub(crate) async fn get(&self) -> Result<MutexGuard<'_, Client>> {
        let mut client = self.client.lock().await;
        if client.is_closed() {
            *client = Self::open(&self.database_url, self.label).await?;
        }
        Ok(client)
    }

    async fn open(database_url: &str, label: &'static str) -> Result<Client> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("fastsearch-pg {label} connection error: {error}");
            }
        });
        Ok(client)
    }

    #[cfg(test)]
    pub(crate) async fn raw(&self) -> MutexGuard<'_, Client> {
        self.client.lock().await
    }

    /// Test-only escape hatch for legacy schema/integration fixtures that execute raw SQL.
    #[cfg(test)]
    pub(crate) async fn lock(&self) -> MutexGuard<'_, Client> {
        self.raw().await
    }
}
