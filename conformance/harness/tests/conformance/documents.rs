//! Document endpoints: DOC-13.

use conformance::mock::Document;
use conformance::runner::{Observations, run};

use crate::common::{error_view, users_gateway};

/// DOC-13: `/document-json` when the upstream document cannot be fetched.
#[tokio::test]
async fn doc_13_document_fetch_fails() {
    run(
        "doc_13_document_fetch_fails",
        &["DOC-13"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let loaded = gw.get("/document-json?type=svc").await?;
            assert_eq!(loaded.status, 200);

            for (label, document) in [
                ("upstream answers 500", Document::Status(500)),
                ("upstream answers 404", Document::Status(404)),
                ("upstream closes the connection", Document::Close),
            ] {
                mock.set_document(document);
                let response = gw.get("/document-json?type=svc").await?;
                assert_eq!(response.status, 500, "{label}");
                assert_eq!(
                    response.text(),
                    r#"{"statusCode":500,"message":"Internal server error"}"#,
                    "{label}"
                );
                assert_eq!(
                    response.header("content-type"),
                    Some("application/json; charset=utf-8"),
                    "{label}"
                );
                obs.record(label, error_view(&response));
            }
            Ok(obs)
        },
    )
    .await;
}
