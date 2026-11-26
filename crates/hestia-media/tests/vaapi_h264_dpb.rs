#![cfg(all(target_os = "linux", feature = "vaapi"))]

use cros_codecs::codec::h264::dpb::Dpb;
use cros_codecs::codec::h264::parser::RefPicMarkingInner;
use cros_codecs::codec::h264::picture::{PictureData, Reference};

#[test]
fn one_slot_dpb_reuses_a_retired_reference_after_immediate_output() {
    let mut dpb = Dpb::default();
    dpb.set_limits(1, 0);
    for number in 0..64 {
        let mut pic = PictureData::default();
        pic.pic_num = number;
        pic.set_reference(Reference::ShortTerm, false);
        if number > 0 {
            // MMCO 1 retires the previous reference after decoding this picture.
            assert_eq!(dpb.entries()[0].reference, Some(number - 1));
            dpb.mmco_op_1(&pic, &RefPicMarkingInner::default()).unwrap();
        }
        assert!(dpb.bump_as_needed(&pic).is_empty());
        dpb.store_picture(pic.into_rc(), Some(number)).unwrap();
        assert_eq!(dpb.output_all(), [Some(number)]);
    }
}

#[test]
fn a_retired_picture_still_waiting_for_display_is_output_once() {
    let mut dpb = Dpb::default();
    dpb.set_limits(1, 0);
    let mut previous = PictureData::default();
    previous.set_reference(Reference::ShortTerm, false);
    dpb.store_picture(previous.into_rc(), Some(0)).unwrap();

    let mut current = PictureData::default();
    current.pic_num = 1;
    current.set_reference(Reference::ShortTerm, false);
    dpb.mmco_op_1(&current, &RefPicMarkingInner::default()).unwrap();
    assert_eq!(dpb.bump_as_needed(&current), [Some(0)]);
    dpb.store_picture(current.into_rc(), Some(1)).unwrap();
    assert_eq!(dpb.output_all(), [Some(1)]);
    assert!(dpb.output_all().is_empty());
}
