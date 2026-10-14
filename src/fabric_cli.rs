//! Browser CLI preflight and a non-mutating status projection.
use crate::fabric_browser::{
    self, BrowserConnectOptions, BrowserOAuth, CredentialRecord, CredentialStore,
    EnrollmentProtocol, EnrollmentStatus, LocalSupervisorInventory, VerifiedIdentity,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub(crate) fn validate_inventory(
    options: &BrowserConnectOptions,
    inventory: &LocalSupervisorInventory,
) -> Result<()> {
    crate::host_identity::validate(&inventory.host_id)?;
    anyhow::ensure!(
        options
            .host_id
            .as_deref()
            .is_none_or(|id| id == inventory.host_id),
        "configured Host differs from the running Supervisor"
    );
    anyhow::ensure!(
        inventory.control_protocol >= crate::session_control::CONTROL_PROTOCOL_VERSION
            && !inventory.boot_generation.is_empty(),
        "Supervisor is not compatible with browser enrollment"
    );
    anyhow::ensure!(
        !inventory.roots.is_empty() && inventory.roots.len() <= 32,
        "Supervisor has no usable configured roots"
    );
    for root in &inventory.roots {
        crate::named_roots::validate_root_name(&root.name)?;
    }
    Ok(())
}

pub(crate) async fn check_options(
    options: &BrowserConnectOptions,
) -> Result<LocalSupervisorInventory> {
    fabric_browser::validate_oauth_options(options)?;
    let inventory = inventory_from_ping(
        &crate::session_control::read_only_local_status()
            .await
            .context("read-only Supervisor diagnostics are unavailable")?,
    )?;
    validate_inventory(options, &inventory)?;
    let listener = fabric_browser::bind_callback_listener(options.oauth_callback_port).await?;
    drop(listener);
    Ok(inventory)
}

fn inventory_from_ping(ping: &Value) -> Result<LocalSupervisorInventory> {
    anyhow::ensure!(ping["status"] == "active", "Supervisor is inactive");
    let host_id = ping["host_id"]
        .as_str()
        .context("Supervisor Host is unavailable")?
        .to_owned();
    let boot_generation = ping["boot_generation"]
        .as_str()
        .context("Supervisor generation is unavailable")?
        .to_owned();
    let control_protocol = ping["control_protocol"]
        .as_u64()
        .context("Supervisor protocol is unavailable")?;
    let names = ping["named_roots"]
        .as_array()
        .context("Supervisor configured roots are unavailable")?;
    let roots = names
        .iter()
        .map(|name| {
            let name = name
                .as_str()
                .context("Supervisor root name is invalid")?
                .to_owned();
            Ok(fabric_browser::LocalRootIdentity {
                name,
                canonical_path: std::path::PathBuf::new(),
                device: 0,
                inode: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(LocalSupervisorInventory {
        host_id,
        boot_generation,
        control_protocol,
        roots,
    })
}

pub async fn run_check(options: BrowserConnectOptions) -> Result<()> {
    let inventory = check_options(&options).await?;
    BrowserOAuth::new(options.clone())?.check_metadata().await?;
    println!(
        "{}",
        json!({"mode":"browser", "ready":true, "host_id":inventory.host_id,
        "configured_roots":inventory.roots.iter().map(|root| &root.name).collect::<Vec<_>>(),
        "callback_port":options.oauth_callback_port, "authentication_started":false,
        "next":"Run temote fabric connect in an interactive terminal; approve the displayed account, Host and selected roots."})
    );
    Ok(())
}

/// The status boundary has no authentication, refresh, enrollment mutation or
/// credential write method. Test peers can prove these reads are sufficient.
#[allow(async_fn_in_trait)]
pub(crate) trait StatusProtocol {
    async fn identity(&self, record: &CredentialRecord) -> Result<VerifiedIdentity>;
    async fn enrollment(&self, record: &CredentialRecord) -> Result<Option<EnrollmentStatus>>;
    async fn link_generation(&self, record: &CredentialRecord) -> Result<Option<u64>>;
}

impl StatusProtocol for BrowserOAuth {
    async fn identity(&self, record: &CredentialRecord) -> Result<VerifiedIdentity> {
        BrowserOAuth::identity(self, &record.oauth_access_token).await
    }
    async fn enrollment(&self, record: &CredentialRecord) -> Result<Option<EnrollmentStatus>> {
        EnrollmentProtocol::status(self, &record.oauth_access_token, &record.host_id).await
    }
    async fn link_generation(&self, record: &CredentialRecord) -> Result<Option<u64>> {
        BrowserOAuth::link_generation(self, record).await
    }
}

pub(crate) async fn status_projection<P: StatusProtocol>(
    options: &BrowserConnectOptions,
    inventory: &LocalSupervisorInventory,
    store: &dyn CredentialStore,
    protocol: &P,
    local_receipt: Option<crate::gateway::BrowserLinkReceipt>,
    verify_local_identity: bool,
) -> Value {
    let origin = url::Url::parse(&options.gateway_url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    let mut result = json!({"service":"temote-fabric", "mode":"browser", "host_id":inventory.host_id,
        "endpoint":options.gateway_url, "ready":false, "authentication":"unavailable",
        "enrollment":"not_checked", "link":"not_checked", "connected":false, "root_identity":"not_checked", "account":null, "approved_roots":[],
        "configured_roots":inventory.roots.iter().map(|root| &root.name).collect::<Vec<_>>(),
        "next":"Run temote fabric connect in an interactive terminal to resume; status never logs in or refreshes."});
    let record = match store.load_for_status() {
        Ok(Some(record)) => record,
        Ok(None) => {
            result["authentication"] = json!("not_signed_in");
            return result;
        }
        Err(_) => return result,
    };
    result["enrollment"] = json!(match record.status {
        fabric_browser::CredentialStatus::Pending => "pending",
        fabric_browser::CredentialStatus::Active => "active",
        fabric_browser::CredentialStatus::Revoking => "revoking",
    });
    if record.host_id != inventory.host_id
        || record.gateway_origin != origin
        || record.issuer != options.issuer
        || record.oauth_client_id != options.client_id
    {
        result["authentication"] = json!("configuration_changed");
        return result;
    }
    if record.refresh_in_flight || record.refresh_outcome_unknown || record.refresh_commit_pending {
        result["authentication"] = json!("refresh_unresolved");
        return result;
    }
    if record.access_expires_at <= fabric_browser::unix_now() {
        result["authentication"] = json!("expired");
        return result;
    }
    let identity = match protocol.identity(&record).await {
        Ok(identity)
            if identity.owner_key == record.owner_key
                && fabric_browser::validate_verified_identity(&identity).is_ok() =>
        {
            identity
        }
        _ => {
            result["authentication"] = json!("rejected_or_unavailable");
            return result;
        }
    };
    result["authentication"] = json!("verified");
    result["account"] = json!({"email":identity.email, "verified":true});
    let roots_available = record
        .approved_roots
        .iter()
        .all(|name| inventory.roots.iter().any(|root| &root.name == name));
    if record.validate_for_link(&origin).is_err() || !roots_available {
        result["enrollment"] = json!("not_ready");
        return result;
    }
    if verify_local_identity {
        if fabric_browser::make_fabric_authority(
            &record,
            &fabric_browser::WorkerAuthSnapshot {
                mode: "browser".into(),
                owner_key: record.owner_key.clone(),
                grant_id: record.grant_id.clone(),
                grant_generation: record.grant_generation,
                approved_roots: record.approved_roots.clone(),
            },
            inventory,
        )
        .is_err()
        {
            result["root_identity"] = json!("changed_or_unavailable");
            return result;
        }
        result["root_identity"] = json!("verified");
    } else {
        result["root_identity"] = json!("unverified");
    }
    let remote = match protocol.enrollment(&record).await {
        Ok(Some(remote))
            if fabric_browser::validate_remote_scope(&remote, &record, "active").is_ok()
                && remote
                    .expires_at
                    .is_some_and(|expiry| expiry > fabric_browser::unix_now()) =>
        {
            remote
        }
        _ => {
            result["enrollment"] = json!("rejected_or_unavailable");
            return result;
        }
    };
    result["approved_roots"] = json!(remote.root_names);
    result["enrollment"] = json!("verified");
    match protocol.link_generation(&record).await {
        Ok(Some(generation))
            if generation > 0
                && local_receipt.as_ref().is_some_and(|receipt| {
                    receipt.generation == generation && receipt.matches(&record)
                }) =>
        {
            result["link"] = json!("online");
            result["connected"] = json!(true);
            result["ready"] = json!(verify_local_identity);
            result["next"] = json!(
                "Keep the connect terminal open. This Host is connected under the displayed approved roots."
            );
        }
        Ok(Some(_)) => result["link"] = json!("generation_unverified"),
        Ok(None) => result["link"] = json!("offline"),
        Err(_) => result["link"] = json!("unavailable"),
    }
    result
}

pub async fn run_status(options: BrowserConnectOptions) -> Result<()> {
    fabric_browser::validate_oauth_options(&options)?;
    let background = crate::fabric_background::projection(&options).await;
    let (inventory, verify_local_identity) =
        match crate::session_control::read_only_fabric_inventory().await {
            Ok(inventory) => (inventory, true),
            Err(_) => match crate::session_control::read_only_local_status()
                .await
                .and_then(|ping| inventory_from_ping(&ping))
            {
                Ok(inventory) => (inventory, false),
                Err(_) => {
                    println!(
                        "{}",
                        json!({"mode":"browser","ready":false,"supervisor":"unavailable", "background":background, "authentication":"not_checked", "link":"not_checked", "next":"Read-only Supervisor diagnostics are unavailable; status will not restart it."})
                    );
                    anyhow::bail!("Fabric status is incomplete; review the diagnostics above");
                }
            },
        };
    validate_inventory(&options, &inventory)?;
    let store =
        fabric_browser::OsCredentialStore::open_profile_for_status(&fabric_browser::profile_key(
            &url::Url::parse(&options.gateway_url)?
                .origin()
                .ascii_serialization(),
            "",
            &inventory.host_id,
        ))?;
    let protocol = BrowserOAuth::new(options.clone())?;
    let mut result = status_projection(
        &options,
        &inventory,
        &store,
        &protocol,
        crate::gateway::read_browser_host_agent_receipt(&inventory.host_id),
        verify_local_identity,
    )
    .await;
    result["background"] = background;
    if !verify_local_identity {
        result["next"] = json!(
            "Link connectivity is available, but read-only root identity needs a newer Supervisor. Review `temote upgrade --dry-run`, then upgrade/restart locally and reconnect."
        );
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    anyhow::ensure!(
        result["ready"] == true,
        "Fabric status is incomplete; review the diagnostics above"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::MetadataExt;

    struct Store(Option<CredentialRecord>);
    impl CredentialStore for Store {
        fn load(&self) -> Result<Option<CredentialRecord>> {
            panic!("interactive lookup must not be used")
        }
        fn load_for_status(&self) -> Result<Option<CredentialRecord>> {
            Ok(self.0.clone())
        }
        fn replace(&self, _: &CredentialRecord) -> Result<()> {
            panic!("status must not write")
        }
        fn delete(&self) -> Result<()> {
            panic!("status must not delete")
        }
    }
    struct Peer {
        fault: u8,
        calls: RefCell<Vec<&'static str>>,
    }
    impl StatusProtocol for Peer {
        async fn identity(&self, record: &CredentialRecord) -> Result<VerifiedIdentity> {
            self.calls.borrow_mut().push("identity");
            if self.fault == 1 {
                anyhow::bail!("REMOTE_SECRET_SENTINEL");
            }
            Ok(VerifiedIdentity {
                owner_key: if self.fault == 2 {
                    "b".repeat(64)
                } else {
                    record.owner_key.clone()
                },
                email: if self.fault == 8 {
                    "bad\nSECRET_SENTINEL".into()
                } else {
                    "owner@example.test".into()
                },
            })
        }
        async fn enrollment(&self, record: &CredentialRecord) -> Result<Option<EnrollmentStatus>> {
            self.calls.borrow_mut().push("enrollment");
            if self.fault == 5 {
                anyhow::bail!("ENROLLMENT_SECRET_SENTINEL");
            }
            Ok(Some(EnrollmentStatus {
                host_id: record.host_id.clone(),
                status: "active".into(),
                generation: record.grant_generation + u64::from(self.fault == 3),
                root_names: if self.fault == 9 {
                    vec!["extra".into()]
                } else {
                    record.approved_roots.clone()
                },
                owner_key: Some(record.owner_key.clone()),
                grant_id: Some(record.grant_id.clone()),
                attempt_id: None,
                reservation_operation_id: None,
                cancel_operation_id: None,
                operation_id: None,
                operation_generation: None,
                expires_at: Some(record.grant_expires_at + u64::from(self.fault == 10)),
                pending_expires_at: None,
            }))
        }
        async fn link_generation(&self, _: &CredentialRecord) -> Result<Option<u64>> {
            self.calls.borrow_mut().push("link");
            if self.fault == 6 {
                anyhow::bail!("LINK_SECRET_SENTINEL");
            }
            Ok(match self.fault {
                4 => None,
                7 => Some(9),
                _ => Some(8),
            })
        }
    }
    fn fixture() -> (
        tempfile::TempDir,
        BrowserConnectOptions,
        LocalSupervisorInventory,
        CredentialRecord,
    ) {
        let root = tempfile::tempdir().unwrap();
        let metadata = root.path().metadata().unwrap();
        let inventory = LocalSupervisorInventory {
            host_id: "host-a".into(),
            boot_generation: "current-boot".into(),
            control_protocol: crate::session_control::CONTROL_PROTOCOL_VERSION,
            roots: vec![fabric_browser::LocalRootIdentity {
                name: "src".into(),
                canonical_path: root.path().to_path_buf(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }],
        };
        let options = BrowserConnectOptions {
            gateway_url: "https://fabric.example".into(),
            issuer: "https://login.example".into(),
            client_id: "public-client".into(),
            host_id: Some("host-a".into()),
            resource: "https://fabric.example".into(),
            oauth_callback_port: None,
            permitted_origins: vec![
                "https://fabric.example".into(),
                "https://login.example".into(),
            ],
        };
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let fingerprints = fabric_browser::inventory_root_fingerprints(&inventory, key).unwrap();
        let registry = fabric_browser::root_registry_fingerprint(key, &fingerprints).unwrap();
        let record = serde_json::from_value(json!({"schema_version":1,"version":1,"gateway_origin":"https://fabric.example","issuer":"https://login.example","oauth_client_id":"public-client","oauth_authorization_endpoint":"https://login.example/authorize","oauth_token_endpoint":"https://login.example/token","owner_key":"a".repeat(64),"email":"owner@example.test","host_id":"host-a","grant_id":"00000000-0000-4000-8000-000000000001","grant_secret":"GRANT_SECRET_SENTINEL","grant_generation":7,"grant_expires_at":9999999999u64,"attempt_id":"attempt","reserve_operation_id":"reserve","expected_generation":6,"activation_operation_id":"activate","cancel_operation_id":null,"revoke_operation_id":null,"oauth_access_token":"oauth:ACCESS_SECRET_SENTINEL","oauth_refresh_token":"REFRESH_SECRET_SENTINEL","access_expires_at":9999999999u64,"refresh_in_flight":false,"refresh_attempt_id":null,"refresh_outcome_unknown":false,"refresh_commit_pending":false,"status":"active","supervisor_boot_generation":"original-boot","root_registry_fingerprint":registry,"root_fingerprints":fingerprints,"approved_roots":["src"],"local_hmac_key":key})).unwrap();
        (root, options, inventory, record)
    }
    #[tokio::test]
    async fn resume_only_checks_never_use_interactive_store_or_mutating_protocol() {
        let (_root, options, inventory, record) = fixture();
        let peer = Peer {
            fault: 0,
            calls: RefCell::new(vec![]),
        };
        let ready = fabric_browser::resume_browser_link(
            &options,
            &inventory,
            &Store(Some(record.clone())),
            &peer,
        )
        .await
        .unwrap();
        assert_eq!(
            ready.authority.supervisor_boot_generation,
            inventory.boot_generation
        );
        assert_eq!(*peer.calls.borrow(), ["identity", "enrollment"]);
        for fault in [1, 2, 3, 5, 8, 9, 10] {
            let peer = Peer {
                fault,
                calls: RefCell::new(vec![]),
            };
            assert!(
                fabric_browser::resume_browser_link(
                    &options,
                    &inventory,
                    &Store(Some(record.clone())),
                    &peer
                )
                .await
                .is_err(),
                "fault {fault}"
            );
        }
        for fault in 0..12 {
            let mut changed = record.clone();
            let mut pinned = options.clone();
            let mut local = inventory.clone();
            match fault {
                0 => changed.status = fabric_browser::CredentialStatus::Pending,
                1 => changed.status = fabric_browser::CredentialStatus::Revoking,
                2 => changed.grant_expires_at = 0,
                3 => changed.access_expires_at = 0,
                4 => changed.refresh_in_flight = true,
                5 => changed.refresh_commit_pending = true,
                6 => changed.refresh_outcome_unknown = true,
                7 => pinned.client_id = "different-public-client".into(),
                8 => pinned.issuer = "https://other.example".into(),
                9 => local.roots[0].inode += 1,
                10 => changed.root_registry_fingerprint = "changed".into(),
                _ => pinned.host_id = Some("other-host".into()),
            }
            assert!(
                fabric_browser::resume_browser_link(&pinned, &local, &Store(Some(changed)), &peer)
                    .await
                    .is_err(),
                "fault {fault}"
            );
        }
        assert!(
            fabric_browser::resume_browser_link(&options, &inventory, &Store(None), &peer)
                .await
                .is_err()
        );
    }

    fn receipt(record: &CredentialRecord) -> crate::gateway::BrowserLinkReceipt {
        crate::gateway::BrowserLinkReceipt {
            generation: 8,
            gateway_origin: record.gateway_origin.clone(),
            grant_id: record.grant_id.clone(),
            grant_generation: record.grant_generation,
        }
    }
    #[tokio::test]
    async fn browser_status_verifies_identity_scope_and_link_without_mutations() {
        let (_root, mut options, inventory, record) = fixture();
        options.gateway_url = "https://FABRIC.example:443/".into();
        let peer = Peer {
            fault: 0,
            calls: RefCell::new(vec![]),
        };
        let result = status_projection(
            &options,
            &inventory,
            &Store(Some(record.clone())),
            &peer,
            Some(receipt(&record)),
            true,
        )
        .await;
        assert_eq!(result["ready"], true);
        assert_eq!(result["approved_roots"], json!(["src"]));
        assert_eq!(result["account"]["email"], "owner@example.test");
        assert_eq!(*peer.calls.borrow(), vec!["identity", "enrollment", "link"]);
        let output = result.to_string();
        assert!(!output.contains("SECRET_SENTINEL") && !output.contains("local_hmac_key"));
        assert!(
            !output.contains(
                &inventory.roots[0]
                    .canonical_path
                    .to_string_lossy()
                    .to_string()
            )
        );
    }
    #[tokio::test]
    async fn rejected_auth_scope_or_unfinished_connection_is_not_ready_and_never_echoes_errors() {
        for fault in 1..=9 {
            let (_root, options, inventory, record) = fixture();
            let peer = Peer {
                fault,
                calls: RefCell::new(vec![]),
            };
            let result = status_projection(
                &options,
                &inventory,
                &Store(Some(record.clone())),
                &peer,
                Some(receipt(&record)),
                true,
            )
            .await;
            assert_eq!(result["ready"], false, "fault {fault}");
            assert!(!result.to_string().contains("SECRET_SENTINEL"));
            if matches!(fault, 1 | 2 | 8) {
                assert!(result["account"].is_null());
            }
        }
    }
    #[tokio::test]
    async fn legacy_or_different_grant_receipts_never_claim_an_online_link() {
        let (_root, options, inventory, record) = fixture();
        let peer = Peer {
            fault: 0,
            calls: RefCell::new(vec![]),
        };
        for mode in 0..4 {
            let mut local = receipt(&record);
            match mode {
                1 => local.grant_id = "00000000-0000-4000-8000-000000000002".into(),
                2 => local.grant_generation += 1,
                3 => local.gateway_origin = "https://other.example".into(),
                _ => {}
            }
            let result = status_projection(
                &options,
                &inventory,
                &Store(Some(record.clone())),
                &peer,
                (mode != 0).then_some(local),
                true,
            )
            .await;
            assert_eq!(result["authentication"], "verified");
            assert_eq!(result["enrollment"], "verified");
            assert_eq!(result["link"], "generation_unverified");
            assert_eq!(result["connected"], false);
            assert_eq!(result["ready"], false);
        }
    }

    #[tokio::test]
    async fn missing_expired_pending_or_uncertain_credentials_never_start_authentication() {
        let (_root, options, inventory, record) = fixture();
        let peer = Peer {
            fault: 0,
            calls: RefCell::new(vec![]),
        };
        let absent = status_projection(&options, &inventory, &Store(None), &peer, None, true).await;
        assert_eq!(absent["authentication"], "not_signed_in");
        assert!(peer.calls.borrow().is_empty());
        for mode in 0..4 {
            let mut changed = record.clone();
            match mode {
                0 => changed.access_expires_at = 1,
                1 => changed.refresh_outcome_unknown = true,
                2 => changed.status = fabric_browser::CredentialStatus::Pending,
                _ => changed.grant_expires_at = 1,
            }
            let result = status_projection(
                &options,
                &inventory,
                &Store(Some(changed)),
                &peer,
                Some(receipt(&record)),
                true,
            )
            .await;
            assert_eq!(result["ready"], false);
        }
        assert!(!peer.calls.borrow().contains(&"enrollment"));
    }
    #[tokio::test]
    async fn old_supervisor_can_report_online_without_claiming_root_identity_ready() {
        let (_root, options, current, record) = fixture();
        let ping = json!({"status":"active","host_id":current.host_id,"boot_generation":current.boot_generation,"control_protocol":current.control_protocol,"named_roots":["src"]});
        let legacy = inventory_from_ping(&ping).unwrap();
        let peer = Peer {
            fault: 0,
            calls: RefCell::new(vec![]),
        };
        let result = status_projection(
            &options,
            &legacy,
            &Store(Some(record.clone())),
            &peer,
            Some(receipt(&record)),
            false,
        )
        .await;
        assert_eq!(result["link"], "online");
        assert_eq!(result["connected"], true);
        assert_eq!(result["root_identity"], "unverified");
        assert_eq!(result["ready"], false);
        assert_eq!(result["account"]["verified"], true);
    }

    #[test]
    fn preflight_fails_closed_for_wrong_host_missing_roots_and_old_protocol() {
        let (_root, mut options, mut inventory, _record) = fixture();
        assert!(validate_inventory(&options, &inventory).is_ok());
        options.host_id = Some("different-host".into());
        assert!(validate_inventory(&options, &inventory).is_err());
        options.host_id = None;
        inventory.control_protocol = 0;
        assert!(validate_inventory(&options, &inventory).is_err());
        inventory.control_protocol = crate::session_control::CONTROL_PROTOCOL_VERSION;
        inventory.roots.clear();
        assert!(validate_inventory(&options, &inventory).is_err());
    }
}
