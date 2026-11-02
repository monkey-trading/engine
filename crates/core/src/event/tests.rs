use super::*;

/// The `kind` tag of a JSON object. Deserializing fails when the object has two `kind` keys.
#[derive(Deserialize)]
struct Tag {
    kind: String,
}

#[test]
fn no_price_rejection_has_one_kind_key_and_round_trips() {
    let cases = [
        (
            PriceError::Stale { fresh: 1, needed: 2 },
            r#"{"kind":"no_price","error":{"kind":"stale","fresh":1,"needed":2}}"#,
        ),
        (
            PriceError::Divergent { divergence_bps: 75 },
            r#"{"kind":"no_price","error":{"kind":"divergent","divergence_bps":75}}"#,
        ),
    ];
    for (error, expected) in cases {
        let reason = Reject::NoPrice { error };
        let json = serde_json::to_string(&reason).expect("rejections serialise");
        assert_eq!(json, expected);
        let tag: Tag = serde_json::from_str(&json).expect("exactly one kind key");
        assert_eq!(tag.kind, "no_price");

        // The event clients receive deserializes back to what the engine emitted.
        let event = Event::OrderRejected { account: AccountId([1; 20]), client_id: 7, reason };
        let json = serde_json::to_string(&event).expect("events serialise");
        assert_eq!(serde_json::from_str::<Event>(&json).expect("events deserialise"), event);
    }
}
