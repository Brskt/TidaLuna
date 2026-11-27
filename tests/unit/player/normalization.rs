use super::*;
use serde_json::json;

#[test]
fn a_factor_from_the_page_is_kept_only_when_it_attenuates() {
    assert_eq!(factor_from_wire(Some(&json!(0.5))), 0.5);
    assert_eq!(factor_from_wire(Some(&json!(1))), 1.0);
    // Anything else plays untouched: a bad value must never silence or boost a track.
    for bad in [json!(0), json!(-0.3), json!(1.7), json!("0.5"), json!(null)] {
        assert_eq!(factor_from_wire(Some(&bad)), 1.0, "{bad}");
    }
    assert_eq!(factor_from_wire(None), 1.0);
}
