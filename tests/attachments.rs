use serde_json::{Value, json};
use std::{io::Cursor, sync::Arc};
use werk_worker::{
    attachments::ImageAttachment, policy::HostPolicy, profiles::AgentProfile, providers::*, *,
};

fn image() -> ImageAttachment {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1600, 1200)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    ImageAttachment::from_bytes(bytes.get_ref()).unwrap()
}
#[test]
fn images_are_bounded_and_text_wire_format_is_unchanged() {
    let image = image();
    image.validate().unwrap();
    assert_eq!((image.width, image.height), (1024, 768));
    assert!(ImageAttachment::from_bytes(b"not an image").is_err());
    let mut invalid = image.clone();
    invalid.width = 99999;
    assert!(invalid.validate().is_err());
    let old: Message = serde_json::from_value(json!({"role":"user","content":"hello"})).unwrap();
    assert!(old.images.is_empty());
    assert_eq!(wire_message(&old), json!({"role":"user","content":"hello"}));
    let mut message = old;
    message.images.push(image.clone());
    let wire = wire_message(&message);
    assert_eq!(wire["content"][0], json!({"type":"text","text":"hello"}));
    assert_eq!(wire["content"][1]["image_url"]["url"], image.data_url());
    let request = ModelRequest {
        messages: vec![message],
        tools: vec![],
        max_output_tokens: 100,
        output_schema: None,
    };
    let estimate = context::estimate_request(&request);
    assert!(
        (8192..9000).contains(&estimate),
        "Image bytes must not be counted as prompt text: {estimate}"
    );
}
#[tokio::test]
async fn image_only_input_is_durable_idempotent_and_reaches_the_model() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = Arc::new(
        sessions::SqliteStore::open(root.path().join("session.sqlite"))
            .await
            .unwrap(),
    );
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text("image received"),
        ScriptedTurn::text("follow-up received"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .store(store.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "vision-test".into(),
                prompt: "Describe input".into(),
                tools: vec![],
                skills: vec![],
            },
            &workspace,
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let picture = image();
    let run = session
        .submit_with_images("", vec![picture.clone()], "image", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let repeated = session
        .submit_with_images("", vec![picture.clone()], "image", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.id, repeated.id);
    assert_eq!(
        provider.requests()[0].messages.last().unwrap().images,
        vec![picture.clone()]
    );
    let followup = session
        .submit("Use the same image", "followup", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(followup.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        provider.requests()[1]
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .images,
        vec![picture.clone()]
    );
    let snapshot = session.snapshot().await.unwrap();
    let id = snapshot.id;
    engine.close().await.unwrap();
    use sessions::SessionStore;
    let reopened = sessions::SqliteStore::open(root.path().join("session.sqlite"))
        .await
        .unwrap();
    let saved = reopened.load(&id).await.unwrap();
    assert_eq!(saved.messages[0].images, vec![picture.clone()]);
    assert_eq!(saved.runs[0].input_images, vec![picture]);
}
