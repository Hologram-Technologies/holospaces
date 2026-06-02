//! `CC-27` — a devcontainer declared with a **Docker Compose** file resolves its
//! service, never silently defaulting.
//!
//! A `devcontainer.json` may declare its container with a `dockerComposeFile` and
//! a `service` (the service that *is* the devcontainer). holospaces honours it by
//! reading the compose file from the repository and resolving that service's image
//! source — its `image` (pulled like `CC-20`) or its `build` (a Dockerfile build,
//! `CC-26`) — and provisioning from it; a missing or ambiguous service is an
//! explicit error, never a silent default.
//!
//! The external authority is the **Compose specification** (the
//! `services.<name>.{image|build}` model) and the Dev Container spec's
//! `dockerComposeFile` + `service`. This witness exercises the import-side
//! resolution from a repository archive (the resolution the import does before
//! pulling the service's image / building its Dockerfile).

use std::path::{Path, PathBuf};

use holospaces::assembly::{find_devcontainer_json, read_archive_file, Layer};
use holospaces::boot::devcontainer::{self, ImageSource};
use holospaces::compose::{self, ServiceSource};

fn art() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts")
}

/// From a repository archive declaring `dockerComposeFile` + `service`, holospaces
/// finds the `devcontainer.json`, parses it to a `Compose` source (retaining the
/// file path + service), reads the compose file from the repository, and resolves
/// the selected service to its image — the resolution the import does, never the
/// silent default.
#[test]
fn the_import_resolves_the_compose_service_from_a_repo() {
    let archive = std::fs::read(art().join("cc27/repo.tar.gz")).unwrap();
    let layer = Layer {
        media_type: "application/gzip",
        blob: &archive,
    };

    // The import finds the devcontainer.json and parses it — `dockerComposeFile`
    // and `service` retained, not dropped.
    let cfg = find_devcontainer_json(&layer)
        .unwrap()
        .expect("devcontainer.json in the repo");
    let dc = devcontainer::parse(&cfg).expect("parse the devcontainer");
    let cc = match &dc.image_source {
        ImageSource::Compose(cc) => cc,
        other => panic!("expected a Compose image source, got {other:?}"),
    };
    assert_eq!(cc.files, vec!["docker-compose.yml".to_owned()]);
    assert_eq!(cc.service.as_deref(), Some("app"));

    // The compose file is read from the repository and the service resolved.
    let compose_bytes = read_archive_file(&layer, ".devcontainer/docker-compose.yml")
        .unwrap()
        .expect("the compose file is read from the repository");
    let source = compose::resolve_service(&compose_bytes, cc.service.as_deref()).expect("resolve");
    assert_eq!(
        source,
        ServiceSource::Image("holospaces/busybox:latest".to_owned()),
        "the `app` service's image is resolved (not the `db` service, not the default)"
    );

    // A service that is not declared is an explicit error (never a silent default).
    assert!(
        compose::resolve_service(&compose_bytes, Some("missing")).is_err(),
        "an undeclared service is refused, not silently defaulted"
    );
}
