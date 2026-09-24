use super::*;
use pretty_assertions::assert_eq;

#[test]
fn accepts_github_app_client_ids_with_dots() {
    assert!(valid_client_id("Iv1.example123"));
    assert!(valid_client_id("Ov23li123"));
    assert!(!valid_client_id(""));
    assert!(!valid_client_id("Iv1.example&scope=admin"));
    assert!(!valid_client_id(&"a".repeat(129)));
}

#[test]
fn accepts_only_copilot_api_hosts_from_token() {
    assert_eq!(
        copilot_base_url("tid=1;proxy-ep=proxy.individual.githubcopilot.com;exp=9").unwrap(),
        "https://api.individual.githubcopilot.com"
    );
    assert!(copilot_base_url("proxy-ep=proxy.evil.example.com").is_err());
    assert!(copilot_base_url("proxy-ep=proxy.githubcopilot.com:444").is_err());
    assert!(copilot_base_url("proxy-ep=proxy.evil.githubcopilot.com/path").is_err());
}

#[test]
fn api_requests_use_github_oauth_token() {
    let credentials = CopilotCredentials {
        github_token: "gho_test".to_string(),
        copilot_token: "tid=1;proxy-ep=proxy.individual.githubcopilot.com".to_string(),
        expires_at: 12345,
        base_url: "https://api.individual.githubcopilot.com".to_string(),
    };

    assert_eq!(credentials.api_token(), "gho_test");
}
