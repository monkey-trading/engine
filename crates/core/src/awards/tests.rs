use super::*;

const RECIPIENT: AccountId = AccountId([0x7e; 20]);

/// An award of `amount` base units for a finding of `severity`, under a well-formed advisory id.
fn approval(severity: Severity, amount: u128) -> AdminCommand {
    AdminCommand::ApproveAward(AwardApproval {
        recipient: RECIPIENT,
        severity,
        advisory: "GHSA-4v2c-9xq8-jm3w".into(),
        amount: UsdcUnits(amount),
    })
}

#[test]
fn the_scale_is_the_decided_amount_for_each_severity() {
    let scale = [
        (Severity::Critical, 250_000_000_000),
        (Severity::High, 50_000_000_000),
        (Severity::Medium, 10_000_000_000),
        (Severity::Low, 1_000_000_000),
    ];
    for (severity, units) in scale {
        assert_eq!(severity.amount(), UsdcUnits(units), "{severity:?}");
        assert_eq!(check_award_vote(&approval(severity, units)), Ok(()), "{severity:?}");
    }
}

#[test]
fn an_award_for_any_other_amount_is_refused() {
    for severity in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low] {
        let scale = severity.amount().0;
        let others = [0, 1, scale - 1, scale + 1, scale * 2, Severity::Critical.amount().0 + 1, u128::MAX];
        for amount in others.into_iter().filter(|amount| *amount != scale) {
            let reason = check_award_vote(&approval(severity, amount)).unwrap_err();
            assert!(reason.contains(&format!("{severity:?} award is {scale} USDC base units")), "{reason}");
        }
    }
    // Another severity's amount is another amount.
    assert!(check_award_vote(&approval(Severity::High, Severity::Critical.amount().0)).is_err());
    assert!(check_award_vote(&approval(Severity::Critical, Severity::High.amount().0)).is_err());
}

#[test]
fn an_award_to_the_zero_address_is_refused() {
    let AdminCommand::ApproveAward(mut award) = approval(Severity::Low, Severity::Low.amount().0) else {
        unreachable!()
    };
    award.recipient = AccountId([0; 20]);
    let reason = check_award_vote(&AdminCommand::ApproveAward(award)).unwrap_err();
    assert_eq!(reason, "an award's recipient is a nonzero address");
}

#[test]
fn an_advisory_id_is_one_to_sixty_four_letters_digits_and_hyphens() {
    let with = |advisory: String| {
        let AdminCommand::ApproveAward(award) = approval(Severity::Low, Severity::Low.amount().0) else {
            unreachable!()
        };
        check_award_vote(&AdminCommand::ApproveAward(AwardApproval { advisory, ..award }))
    };
    for good in ["G", "CVE-2026-12345", "GHSA-4v2c-9xq8-jm3w", &"a".repeat(MAX_ADVISORY_CHARS)] {
        assert_eq!(with(good.into()), Ok(()), "{good:?}");
    }
    let long = "a".repeat(MAX_ADVISORY_CHARS + 1);
    for bad in ["", " GHSA-1", "GHSA 1", "GHSA_1", "GHSA-1\n", "<b>", "GHSA-é", &long] {
        assert!(with(bad.into()).is_err(), "{bad:?}");
    }
}

#[test]
fn a_cap_is_accepted_only_within_its_bounds() {
    let cap =
        |excess_bps, max| check_award_vote(&AdminCommand::SetAwardCap(AwardCap { excess_bps, max: UsdcUnits(max) }));
    for (bps, max) in [(500, 50_000_000_000), (2_000, 250_000_000_000), (1_000, 100_000_000_000)] {
        assert_eq!(cap(bps, max), Ok(()), "{bps} {max}");
    }
    for (bps, max) in [(499, 50_000_000_000), (2_001, 50_000_000_000), (0, 50_000_000_000), (-500, 50_000_000_000)] {
        assert!(cap(bps, max).unwrap_err().contains("share"), "{bps}");
    }
    for max in [49_999_999_999, 250_000_000_001, 0, u128::MAX] {
        assert!(cap(500, max).unwrap_err().contains("limit"), "{max}");
    }
    assert_eq!(check_award_vote(&AdminCommand::SetAwardCap(LAUNCH_CAP)), Ok(()));
}

#[test]
fn the_cap_is_the_lesser_of_its_share_of_the_excess_and_its_limit_rounded_down() {
    let launch = LAUNCH_CAP;
    // 5% of 999,999.999999 USDC is 49,999.99999995, which rounds down to a whole base unit.
    assert_eq!(launch.of(999_999_999_999), 49_999_999_999);
    assert_eq!(launch.of(1_000_000_000_000), 50_000_000_000);
    // Past a million USDC of excess the limit binds.
    assert_eq!(launch.of(2_000_000_000_000), 50_000_000_000);
    assert_eq!(launch.of(19), 0);
    assert_eq!(launch.of(20), 1);
    assert_eq!(launch.of(0), 0);
    let widest = AwardCap { excess_bps: 2_000, max: UsdcUnits(250_000_000_000) };
    assert_eq!(widest.of(1_000_000_000_000), 200_000_000_000);
    assert_eq!(widest.of(u128::MAX), 250_000_000_000);
    assert_eq!(AwardCap { excess_bps: 2_000, max: UsdcUnits(u128::MAX) }.of(u128::MAX), u128::MAX / 5);
}

#[test]
fn every_other_command_passes_the_award_check() {
    assert_eq!(check_award_vote(&AdminCommand::ClearNotice), Ok(()));
    assert_eq!(check_award_vote(&AdminCommand::SetReduceOnly { market: None, reduce_only: true }), Ok(()));
}
