//! A member whose config does not name the carried genesis its copy holds refuses its turn before the keys move,
//! where its start would refuse to serve once the incumbent had handed over (`carry::boot::check_copy`).

use super::*;
use crate::config::CarrySection;

/// The dev copy carried nothing, so a config naming `[genesis.carry]` is refused at the turn: the member pages
/// Critical `carry_refused`, asks the incumbent for nothing and leaves the keys where they are.
#[tokio::test]
async fn a_member_whose_config_names_another_carry_than_its_copy_refuses_its_turn_before_the_keys_move() {
    let (_guard, mut alerts) = capture_alerts();
    let (mut fixture, _operator, served, asked) = in_own_turn("member-carry-refused", Vec::new()).await;
    fixture.cfg.genesis.carry = Some(CarrySection { vault: AccountId([0xa1; 20]), file: "carry.json".into() });
    let mut member = fixture.member();
    let refused = member.until_turn().await.err().expect("the member took a turn its start would refuse");
    served.abort();
    assert!(format!("{refused:#}").contains("carried nothing"), "{refused:#}");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the member asked the incumbent for its tail");
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert["alert"] == "carry_refused")
        .map(|alert| alert["level"].clone())
        .collect();
    assert_eq!(paged, ["ERROR"]);
}
