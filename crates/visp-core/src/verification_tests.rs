use super::*;

#[test]
fn started_is_not_final_and_has_no_verdict() {
    let update = VerificationUpdate::started(1, 3);

    assert_eq!(update.stage, VerificationStage::Started);
    assert_eq!(update.round, 1);
    assert_eq!(update.max_rounds, 3);
    assert!(!update.is_final);
    assert_eq!(update.passed, None);
    assert!(update.evidence.is_empty());
    assert!(update.summary.contains('1'));
}

#[test]
fn passed_is_final_and_passed_true() {
    let evidence = vec![VerificationEvidence {
        check: "cargo test".into(),
        method: "run in workspace".into(),
        result: "all green".into(),
    }];

    let update = VerificationUpdate::passed(2, 3, "验证通过", evidence.clone());

    assert_eq!(update.stage, VerificationStage::Passed);
    assert_eq!(update.round, 2);
    assert_eq!(update.max_rounds, 3);
    assert_eq!(update.summary, "验证通过");
    assert_eq!(update.evidence, evidence);
    assert!(update.is_final);
    assert_eq!(update.passed, Some(true));
}

#[test]
fn failed_non_final_is_not_final_but_has_false_verdict() {
    let update = VerificationUpdate::failed(1, 3, "未通过", Vec::new(), false);

    assert_eq!(update.stage, VerificationStage::Failed);
    assert!(!update.is_final);
    assert_eq!(update.passed, Some(false));
}

#[test]
fn failed_final_is_final() {
    let update = VerificationUpdate::failed(3, 3, "未通过", Vec::new(), true);

    assert_eq!(update.stage, VerificationStage::Failed);
    assert!(update.is_final);
    assert_eq!(update.passed, Some(false));
}

#[test]
fn exhausted_is_final_and_passed_false() {
    let update = VerificationUpdate::exhausted(3, 3, "返工轮数用尽", Vec::new());

    assert_eq!(update.stage, VerificationStage::Exhausted);
    assert!(update.is_final);
    assert_eq!(update.passed, Some(false));
}

#[test]
fn rework_is_not_final_and_has_no_verdict() {
    let update = VerificationUpdate::rework(1, 3, "交回返工");

    assert_eq!(update.stage, VerificationStage::Rework);
    assert!(!update.is_final);
    assert_eq!(update.passed, None);
}

#[test]
fn test_inconclusive_is_final_failure_but_not_a_defect_verdict() {
    let update =
        VerificationUpdate::inconclusive(1, 2, "验证者未提交结构化结论（步数用尽）", Vec::new());
    assert_eq!(update.stage, VerificationStage::Inconclusive);
    assert!(update.is_final);
    assert_eq!(update.passed, Some(false), "没做成也绝不能当作通过");
}
