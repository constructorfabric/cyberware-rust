use toolkit_canonical_errors::Problem;

use crate::domain::error::{Dependency, DomainError};

#[test]
fn a_domain_error_renders_as_a_problem_with_the_canonical_status() {
    let problem = Problem::from(DomainError::NotReady {
        dependency: Dependency::Coordination,
    });
    assert_eq!(problem.status, Some(503));
    let rendered = serde_json::to_string(&problem).expect("json");
    assert!(rendered.contains("NOT_READY"), "{rendered}");
    assert!(!rendered.contains("backtrace"), "{rendered}");
}
