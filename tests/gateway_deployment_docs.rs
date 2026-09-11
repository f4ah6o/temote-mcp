const DOCS: [&str; 2] = ["docs/gateway.md", "docs/gateway.ja.md"];

const REQUIRED_TOKENS: [&str; 9] = [
    "workers_dev = false",
    "No targets deployed",
    "custom_domain = true",
    "--keep-vars",
    "--routes",
    "wrangler deployments status",
    "/healthz",
    "Rollback",
    "Deployment target",
];

#[test]
fn gateway_deployment_target_docs_cover_the_operator_contract() {
    for path in DOCS {
        let document = std::fs::read_to_string(path).expect("gateway doc should be readable");
        for token in REQUIRED_TOKENS {
            assert!(
                document.contains(token),
                "{path} is missing deployment target guidance: {token:?}"
            );
        }
        assert!(
            !document.contains("workers_dev = true"),
            "{path} must not recommend enabling workers.dev"
        );
    }
}
