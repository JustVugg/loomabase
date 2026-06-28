use std::sync::Arc;

use loomabase::Result;
use loomabase::client::{SqliteClient, Todo};
use loomabase::crdt::CrdtState;
use tokio::sync::Mutex;

async fn sync(client: &SqliteClient, server: &Arc<Mutex<CrdtState>>) -> Result<()> {
    let server = Arc::clone(server);
    client
        .sync_with(move |payload| async move {
            let device_id = payload.source_device_id.clone();
            server.lock().await.merge(payload, &device_id)
        })
        .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let device_a = SqliteClient::open(":memory:", "device-a").await?;
    let device_b = SqliteClient::open(":memory:", "device-b").await?;
    let server = Arc::new(Mutex::new(CrdtState::default()));

    device_a
        .create_todo("todo-1".into(), "Initial title".into(), false)
        .await?;
    sync(&device_a, &server).await?;
    sync(&device_b, &server).await?;

    device_a
        .update_title("todo-1".into(), "Edited offline on A".into())
        .await?;
    device_b.update_completed("todo-1".into(), true).await?;

    sync(&device_a, &server).await?;
    sync(&device_b, &server).await?;
    sync(&device_a, &server).await?;

    let expected = Todo {
        id: "todo-1".into(),
        title: "Edited offline on A".into(),
        completed: true,
    };
    assert_eq!(
        device_a.get_todo("todo-1".into()).await?,
        Some(expected.clone())
    );
    assert_eq!(device_b.get_todo("todo-1".into()).await?, Some(expected));

    println!("Both offline devices converged without losing either column update.");
    Ok(())
}
