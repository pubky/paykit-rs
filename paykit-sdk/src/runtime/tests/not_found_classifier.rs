use pubky::{errors::RequestError, Error as PubkyError, StatusCode};

use crate::runtime::is_pubky_not_found;

// CLAUDE.md contract: public reads treat 404/GONE as absence, never as errors.
fn server_error(status: StatusCode) -> PubkyError {
    PubkyError::Request(RequestError::Server {
        status,
        message: "test response".into(),
    })
}

#[test]
fn test_is_pubky_not_found_matches_not_found_and_gone() {
    assert!(is_pubky_not_found(&server_error(StatusCode::NOT_FOUND)));
    assert!(is_pubky_not_found(&server_error(StatusCode::GONE)));
}

#[test]
fn test_is_pubky_not_found_rejects_other_statuses_and_variants() {
    assert!(!is_pubky_not_found(&server_error(
        StatusCode::INTERNAL_SERVER_ERROR
    )));
    assert!(!is_pubky_not_found(&server_error(StatusCode::FORBIDDEN)));

    let validation_error = PubkyError::Request(RequestError::Validation {
        message: "invalid request".into(),
    });
    assert!(!is_pubky_not_found(&validation_error));
}
