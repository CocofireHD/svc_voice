use crate::opus::silk::define::SILK_NO_ERROR;
use crate::opus::silk::lin2log::silk_lin2log;
use crate::opus::silk::structs::SilkEncoderState;
use crate::opus::silk::tuning_parameters::VARIABLE_HP_MIN_CUTOFF_HZ;
use crate::opus::silk::vad::silk_vad_init;

pub fn silk_init_encoder(ps_enc: &mut SilkEncoderState, _arch: i32) -> i32 {
    let mut ret = SILK_NO_ERROR;

    ps_enc.s_cmn.variable_hp_smth1_q15 = silk_lin2log(VARIABLE_HP_MIN_CUTOFF_HZ) << 8;
    ps_enc.s_cmn.variable_hp_smth2_q15 = ps_enc.s_cmn.variable_hp_smth1_q15;

    ps_enc.s_cmn.first_frame_after_reset = 1;

    ret += silk_vad_init(&mut ps_enc.s_cmn.s_vad);

    ret
}
