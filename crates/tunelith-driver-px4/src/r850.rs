// SPDX-License-Identifier: GPL-2.0-only
//! Rafael Micro R850, the ISDB-T tuner of the PX4 series and its kin,
//! reached through the demodulator in front of it.
//!
//! Ported from `r850.c` of px4_drv, Copyright (c) 2018-2021 nns779.
//! Some features are not implemented there, and so not here either.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::{I2c, Result};

use crate::error;

const NUM_REGS: usize = 0x30;

#[derive(Clone, Copy)]
pub struct R850Config {
    /// The crystal in kHz; the PLL maths assume 24000.
    pub xtal: u32,
    pub loop_through: bool,
    pub clock_out: bool,
    pub no_imr_calibration: bool,
    pub no_lpf_calibration: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum System {
    DvbT,
    DvbT2,
    DvbT2_1,
    DvbC,
    J83b,
    IsdbT,
    Dtmb,
    Atsc,
    Fm,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bandwidth {
    M6,
    M7,
    M8,
}

use Bandwidth::{M6, M7, M8};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemConfig {
    pub system: System,
    pub bandwidth: Bandwidth,
    /// The intermediate frequency in kHz.
    pub if_freq: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Gain,
    Phase,
}

#[derive(Clone, Copy, Default)]
struct Imr {
    gain: u8,
    phase: u8,
    iqcap: u8,
    value: u8,
}

#[derive(Clone, Copy, Default)]
struct ImrCal {
    imr: [Imr; 5],
    done: bool,
    result: [bool; 5],
    mixer_amp_lpf: u8,
}

#[derive(Clone, Copy)]
struct Lpf {
    code: u8,
    bandwidth: u8,
    lsb: u8,
}

#[derive(Clone, Copy)]
struct SystemParams {
    bandwidth: Bandwidth,
    if_freq: u32,
    filt_cal_if: u32,
    bw: u8,
    filt_ext_ena: u8,
    hpf_notch: u8,
    hpf_cor: u8,
    filt_comp: u8,
    img_gain: u8,
    agc_clk: u8,
    lpf: Lpf,
}

/// A row as laid out in C: the seven fields from `bw`, then the LPF.
const fn sp(bandwidth: Bandwidth, if_freq: u32, filt_cal_if: u32, v: [u8; 10]) -> SystemParams {
    SystemParams {
        bandwidth,
        if_freq,
        filt_cal_if,
        bw: v[0],
        filt_ext_ena: v[1],
        hpf_notch: v[2],
        hpf_cor: v[3],
        filt_comp: v[4],
        img_gain: v[5],
        agc_clk: v[6],
        lpf: Lpf {
            code: v[7],
            bandwidth: v[8],
            lsb: v[9],
        },
    }
}

#[derive(Clone, Copy)]
struct FreqParams {
    if_freq: u32,
    rf_freq_min: u32,
    rf_freq_max: u32,
    lna_top: u8,
    lna_vtl_h: u8,
    lna_nrb_det: u8,
    lna_rf_dis_mode: u8,
    lna_rf_charge_cur: u8,
    lna_rf_dis_curr: u8,
    lna_dis_slow_fast: u8,
    rf_top: u8,
    rf_vtl_h: u8,
    rf_gain_limit: u8,
    rf_dis_slow_fast: u8,
    rf_lte_psg: u8,
    nrb_top: u8,
    nrb_bw_hpf: u8,
    nrb_bw_lpf: u8,
    mixer_top: u8,
    mixer_vth: u8,
    mixer_vtl: u8,
    mixer_amp_lpf: u8,
    mixer_gain_limit: u8,
    mixer_detbw_lpf: u8,
    mixer_filter_dis: u8,
    filter_top: u8,
    filter_vth: u8,
    filter_vtl: u8,
    filt_3th_lpf_cur: u8,
    filt_3th_lpf_gain: u8,
    bb_dis_curr: u8,
    bb_det_mode: u8,
    na_pwr_det: u8,
    enb_poly_gain: u8,
    img_nrb_adder: u8,
    hpf_comp: u8,
    fb_res_1st: u8,
}

/// A row as laid out in C: the frequencies, then the rest in field order.
const fn fp(if_freq: u32, rf_freq_min: u32, rf_freq_max: u32, v: [u8; 34]) -> FreqParams {
    FreqParams {
        if_freq,
        rf_freq_min,
        rf_freq_max,
        lna_top: v[0],
        lna_vtl_h: v[1],
        lna_nrb_det: v[2],
        lna_rf_dis_mode: v[3],
        lna_rf_charge_cur: v[4],
        lna_rf_dis_curr: v[5],
        lna_dis_slow_fast: v[6],
        rf_top: v[7],
        rf_vtl_h: v[8],
        rf_gain_limit: v[9],
        rf_dis_slow_fast: v[10],
        rf_lte_psg: v[11],
        nrb_top: v[12],
        nrb_bw_hpf: v[13],
        nrb_bw_lpf: v[14],
        mixer_top: v[15],
        mixer_vth: v[16],
        mixer_vtl: v[17],
        mixer_amp_lpf: v[18],
        mixer_gain_limit: v[19],
        mixer_detbw_lpf: v[20],
        mixer_filter_dis: v[21],
        filter_top: v[22],
        filter_vth: v[23],
        filter_vtl: v[24],
        filt_3th_lpf_cur: v[25],
        filt_3th_lpf_gain: v[26],
        bb_dis_curr: v[27],
        bb_det_mode: v[28],
        na_pwr_det: v[29],
        enb_poly_gain: v[30],
        img_nrb_adder: v[31],
        hpf_comp: v[32],
        fb_res_1st: v[33],
    }
}

const INIT_REGS: [u8; NUM_REGS] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xca, 0xc0, 0x72, 0x50, 0x00, 0xe0, 0x00, 0x30,
    0x86, 0xbb, 0xf8, 0xb0, 0xd2, 0x81, 0xcd, 0x46, 0x37, 0x40, 0x89, 0x8c, 0x55, 0x95, 0x07, 0x23,
    0x21, 0xf1, 0x4c, 0x5f, 0xc4, 0x20, 0xa9, 0x6c, 0x53, 0xab, 0x5b, 0x46, 0xb3, 0x93, 0x6e, 0x41,
];

const IMR_CAL_REGS: [u8; NUM_REGS] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x49, 0x3a, 0x90, 0x03, 0xc1, 0x61, 0x71,
    0x17, 0xf1, 0x18, 0x55, 0x30, 0x20, 0xf3, 0xed, 0x1f, 0x1c, 0x81, 0x13, 0x00, 0x80, 0x0a, 0x07,
    0x21, 0x71, 0x54, 0xf1, 0xf2, 0xa9, 0xbb, 0x0b, 0xa3, 0xf6, 0x0b, 0x44, 0x92, 0x17, 0xe6, 0x80,
];

const LPF_CAL_REGS: [u8; NUM_REGS] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x49, 0x3f, 0x90, 0x13, 0xe1, 0x89, 0x7a,
    0x07, 0xf1, 0x9a, 0x50, 0x30, 0x20, 0xe1, 0x00, 0x00, 0x04, 0x81, 0x11, 0xef, 0xee, 0x17, 0x07,
    0x31, 0x71, 0x54, 0xb2, 0xee, 0xa9, 0xbb, 0x0b, 0xa3, 0x00, 0x0b, 0x44, 0x92, 0x1f, 0xe6, 0x80,
];

const DVB_T_T2_PARAMS: [[SystemParams; 6]; 2] = [
    [
        sp(M6, 4570, 7550, [1, 0, 0, 0x08, 1, 0, 0, 0x01, 3, 1]),
        sp(M7, 4570, 7920, [1, 0, 0, 0x0b, 1, 0, 0, 0x04, 2, 0]),
        sp(M8, 4570, 8450, [0, 0, 0, 0x0c, 1, 0, 0, 0x01, 2, 0]),
        sp(M6, 5000, 7920, [1, 0, 0, 0x06, 1, 0, 0, 0x06, 2, 1]),
        sp(M7, 5000, 8450, [0, 0, 0, 0x09, 1, 0, 0, 0x00, 2, 1]),
        sp(M8, 5000, 8700, [0, 0, 0, 0x0a, 1, 0, 0, 0x06, 0, 1]),
    ],
    [
        sp(M6, 4570, 7550, [1, 0, 0, 0x08, 1, 3, 1, 0x01, 3, 1]),
        sp(M7, 4570, 7920, [1, 0, 0, 0x0b, 1, 3, 1, 0x04, 2, 0]),
        sp(M8, 4570, 8450, [0, 0, 0, 0x0c, 1, 3, 1, 0x01, 2, 0]),
        sp(M6, 5000, 7920, [1, 0, 0, 0x06, 1, 3, 1, 0x06, 2, 1]),
        sp(M7, 5000, 8450, [0, 0, 0, 0x09, 1, 3, 1, 0x00, 2, 1]),
        sp(M8, 5000, 8700, [0, 0, 0, 0x0a, 1, 3, 1, 0x06, 0, 1]),
    ],
];

const DVB_T2_1_PARAMS: [[SystemParams; 2]; 2] = [
    [
        sp(M7, 1900, 7920, [1, 0, 0, 0x08, 1, 0, 0, 0x04, 2, 0]),
        sp(M7, 5000, 6000, [2, 0, 0, 0x01, 1, 0, 0, 0x0b, 3, 1]),
    ],
    [
        sp(M7, 1900, 7920, [1, 0, 0, 0x08, 1, 3, 1, 0x04, 2, 0]),
        sp(M7, 5000, 6000, [2, 0, 0, 0x01, 1, 3, 1, 0x0b, 3, 1]),
    ],
];

const DVB_C_PARAMS: [[SystemParams; 4]; 2] = [
    [
        sp(M6, 5070, 8100, [1, 0, 0, 0x05, 1, 0, 0, 0x02, 2, 0]),
        sp(M8, 5070, 9550, [0, 0, 0, 0x0b, 1, 0, 0, 0x04, 0, 0]),
        sp(M6, 5000, 7780, [1, 0, 0, 0x06, 1, 0, 0, 0x01, 2, 1]),
        sp(M8, 5000, 9250, [0, 0, 0, 0x0b, 1, 0, 0, 0x05, 0, 1]),
    ],
    [
        sp(M6, 5070, 8100, [1, 0, 0, 0x05, 1, 3, 1, 0x02, 2, 0]),
        sp(M8, 5070, 9550, [0, 0, 0, 0x0b, 1, 3, 1, 0x04, 0, 0]),
        sp(M6, 5000, 7780, [1, 0, 0, 0x06, 1, 3, 1, 0x01, 2, 1]),
        sp(M8, 5000, 9250, [0, 0, 0, 0x0b, 1, 3, 1, 0x05, 0, 1]),
    ],
];

const J83B_PARAMS: [[SystemParams; 2]; 2] = [
    [
        sp(M6, 5070, 8100, [1, 0, 0, 0x05, 1, 0, 0, 0x03, 2, 1]),
        sp(M6, 5000, 7550, [1, 0, 0, 0x05, 1, 0, 0, 0x05, 2, 1]),
    ],
    [
        sp(M6, 5070, 8100, [1, 0, 0, 0x05, 1, 3, 1, 0x03, 2, 1]),
        sp(M6, 5000, 7550, [1, 0, 0, 0x05, 1, 3, 1, 0x05, 2, 1]),
    ],
];

const ISDB_T_PARAMS: [[SystemParams; 3]; 2] = [
    [
        sp(M6, 4063, 7070, [1, 0, 0, 0x08, 1, 0, 0, 0x02, 3, 1]),
        sp(M6, 4570, 7400, [1, 0, 0, 0x05, 1, 0, 0, 0x08, 2, 0]),
        sp(M6, 5000, 7780, [1, 1, 0, 0x03, 1, 0, 0, 0x05, 2, 0]),
    ],
    [
        sp(M6, 4063, 7070, [1, 0, 0, 0x0a, 1, 3, 1, 0x02, 3, 1]),
        sp(M6, 4570, 7400, [1, 0, 0, 0x08, 1, 3, 1, 0x08, 2, 0]),
        sp(M6, 5000, 7780, [1, 0, 0, 0x03, 1, 3, 1, 0x05, 2, 0]),
    ],
];

const DTMB_PARAMS: [[SystemParams; 4]; 2] = [
    [
        sp(M6, 4500, 7200, [1, 0, 0, 0x08, 1, 0, 0, 0x02, 3, 1]),
        sp(M8, 4570, 8450, [0, 0, 0, 0x0c, 1, 0, 0, 0x00, 2, 1]),
        sp(M6, 5000, 8100, [1, 0, 0, 0x06, 1, 0, 0, 0x04, 2, 1]),
        sp(M8, 5000, 8800, [0, 0, 0, 0x0b, 2, 0, 0, 0x05, 0, 1]),
    ],
    [
        sp(M6, 4500, 7200, [1, 0, 0, 0x08, 1, 3, 1, 0x02, 3, 1]),
        sp(M8, 4570, 8450, [0, 0, 0, 0x0c, 1, 3, 1, 0x00, 2, 1]),
        sp(M6, 5000, 8100, [1, 0, 0, 0x06, 1, 3, 1, 0x04, 2, 1]),
        sp(M8, 5000, 8800, [0, 0, 0, 0x0b, 2, 3, 1, 0x05, 0, 1]),
    ],
];

const ATSC_PARAMS: [[SystemParams; 2]; 2] = [
    [
        sp(M6, 5070, 8050, [1, 0, 0, 0x05, 1, 0, 0, 0x03, 2, 0]),
        sp(M6, 5000, 7920, [1, 0, 0, 0x05, 1, 0, 0, 0x04, 2, 0]),
    ],
    [
        sp(M6, 5070, 8050, [1, 0, 0, 0x05, 1, 3, 1, 0x03, 2, 0]),
        sp(M6, 5000, 7920, [1, 0, 0, 0x05, 1, 3, 1, 0x04, 2, 0]),
    ],
];

const DVB_T_T2_FREQ_PARAMS: [FreqParams; 4] = [
    fp(
        0,
        0,
        340000,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x5a, 0, 0x05, 1, 5, 0, 2, 9, 0x09, 0x04, 4, 3, 0, 2, 4,
            0x09, 0x04, 1, 3, 0, 0, 1, 0, 2, 1, 1,
        ],
    ),
    fp(
        0,
        662001,
        670000,
        [
            4, 0x5a, 0, 4, 1, 1, 0x05, 4, 0x5a, 0, 0x05, 1, 4, 0, 2, 9, 0x09, 0x04, 4, 3, 0, 2, 4,
            0x09, 0x04, 1, 3, 0, 0, 1, 0, 2, 1, 1,
        ],
    ),
    fp(
        0,
        782001,
        790000,
        [
            5, 0x5a, 0, 2, 0, 1, 0x05, 4, 0x5a, 0, 0x05, 1, 4, 0, 2, 9, 0x09, 0x04, 4, 3, 0, 2, 4,
            0x09, 0x04, 1, 3, 0, 0, 1, 0, 2, 1, 1,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            4, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x5a, 0, 0x05, 1, 4, 0, 2, 9, 0x09, 0x04, 4, 3, 0, 2, 4,
            0x09, 0x04, 1, 3, 0, 0, 1, 0, 2, 1, 1,
        ],
    ),
];

const DVB_C_FREQ_PARAMS: [FreqParams; 2] = [
    fp(
        0,
        0,
        660000,
        [
            4, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 0, 5, 0, 2, 12, 0x09, 0x04, 4, 2, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 1, 2, 1, 1,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            4, 0x5a, 0, 1, 1, 1, 0x05, 3, 0x4a, 0, 0x05, 0, 5, 0, 2, 12, 0x09, 0x04, 4, 2, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 1, 1, 1, 1,
        ],
    ),
];

const J83B_FREQ_PARAMS: [FreqParams; 3] = [
    fp(
        0,
        0,
        335000,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 0, 5, 0, 0, 12, 0x09, 0x04, 7, 2, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 1, 2, 1, 1,
        ],
    ),
    fp(
        0,
        340001,
        660000,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 0, 5, 0, 0, 12, 0x09, 0x04, 7, 2, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 1, 2, 1, 1,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            4, 0x5a, 0, 1, 1, 1, 0x05, 3, 0x4a, 0, 0x05, 0, 5, 0, 0, 12, 0x09, 0x04, 7, 2, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 1, 1, 1, 1,
        ],
    ),
];

const ISDB_T_FREQ_PARAMS: [FreqParams; 10] = [
    fp(
        4063,
        0,
        340000,
        [
            5, 0x6b, 0, 1, 1, 1, 0x05, 5, 0x4a, 0, 0x05, 1, 12, 0, 2, 15, 0x09, 0x04, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 0, 2, 2, 1,
        ],
    ),
    fp(
        4063,
        470000,
        487999,
        [
            6, 0x8c, 0, 1, 1, 1, 0x05, 5, 0x6b, 0, 0x05, 1, 3, 0, 2, 14, 0x09, 0x04, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 1, 1, 3, 2, 1,
        ],
    ),
    fp(
        4063,
        680000,
        691999,
        [
            5, 0x5a, 0, 2, 1, 1, 0x07, 6, 0x6b, 0, 0x04, 1, 3, 0, 2, 14, 0x09, 0x05, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 0, 1, 3, 2, 1,
        ],
    ),
    fp(
        4063,
        692000,
        697999,
        [
            5, 0x5b, 0, 2, 1, 1, 0x07, 6, 0x6b, 0, 0x04, 1, 10, 0, 3, 12, 0x09, 0x05, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 0, 1, 2, 2, 1,
        ],
    ),
    fp(
        4063,
        0,
        0,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 6, 0x6b, 0, 0x05, 1, 3, 0, 2, 14, 0x09, 0x04, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 1, 1, 3, 2, 1,
        ],
    ),
    fp(
        0,
        0,
        340000,
        [
            5, 0x6b, 0, 1, 1, 1, 0x05, 5, 0x4a, 0, 0x05, 1, 12, 0, 2, 15, 0x0b, 0x06, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 0, 1, 0, 1, 0, 2, 2, 1,
        ],
    ),
    fp(
        0,
        470000,
        487999,
        [
            5, 0x5a, 0, 2, 1, 1, 0x07, 6, 0x6b, 0, 0x04, 1, 3, 0, 2, 14, 0x09, 0x05, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 0, 1, 3, 2, 1,
        ],
    ),
    fp(
        0,
        680000,
        691999,
        [
            5, 0x5b, 0, 2, 1, 1, 0x07, 6, 0x6b, 0, 0x04, 1, 10, 0, 3, 12, 0x09, 0x05, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 0, 1, 2, 2, 1,
        ],
    ),
    fp(
        0,
        692000,
        697999,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 6, 0x6b, 0, 0x05, 1, 3, 0, 2, 14, 0x09, 0x04, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 1, 1, 3, 2, 1,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            5, 0x5a, 0, 1, 1, 1, 0x05, 6, 0x6b, 0, 0x05, 1, 3, 0, 2, 14, 0x09, 0x04, 7, 3, 0, 0,
            12, 0x09, 0x04, 1, 3, 1, 0, 1, 1, 3, 2, 1,
        ],
    ),
];

const DTMB_FREQ_PARAMS: [FreqParams; 3] = [
    fp(
        0,
        0,
        100000,
        [
            4, 0x6b, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 1, 10, 3, 3, 9, 0x09, 0x04, 4, 1, 0, 2, 4,
            0x09, 0x04, 0, 0, 0, 0, 1, 0, 1, 0, 0,
        ],
    ),
    fp(
        0,
        0,
        340000,
        [
            4, 0x6b, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 1, 10, 0, 2, 9, 0x09, 0x04, 4, 1, 0, 2, 4,
            0x09, 0x04, 0, 0, 0, 0, 1, 0, 1, 0, 0,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            4, 0x5a, 0, 1, 1, 1, 0x05, 4, 0x4a, 0, 0x05, 1, 6, 3, 2, 9, 0x09, 0x04, 4, 1, 0, 2, 4,
            0x09, 0x04, 0, 3, 0, 0, 1, 0, 0, 0, 0,
        ],
    ),
];

const ATSC_FREQ_PARAMS: [FreqParams; 2] = [
    fp(
        0,
        0,
        340000,
        [
            6, 0x5a, 0, 1, 1, 1, 0x05, 5, 0x6b, 0, 0x05, 1, 12, 2, 2, 12, 0x0b, 0x04, 7, 2, 1, 2,
            6, 0x09, 0x04, 1, 0, 0, 0, 1, 0, 1, 2, 1,
        ],
    ),
    fp(
        0,
        0,
        0,
        [
            6, 0x5a, 0, 1, 1, 1, 0x05, 5, 0x6b, 0, 0x05, 1, 12, 2, 2, 12, 0x0b, 0x04, 7, 2, 1, 2,
            6, 0x09, 0x04, 1, 3, 0, 0, 1, 0, 1, 2, 1,
        ],
    ),
];

/// The system parameters for chip `chip` (0 or 1).
fn system_params(system: System, chip: usize) -> &'static [SystemParams] {
    match system {
        System::DvbT | System::DvbT2 => &DVB_T_T2_PARAMS[chip],
        System::DvbT2_1 => &DVB_T2_1_PARAMS[chip],
        System::DvbC => &DVB_C_PARAMS[chip],
        System::J83b => &J83B_PARAMS[chip],
        System::IsdbT => &ISDB_T_PARAMS[chip],
        System::Dtmb => &DTMB_PARAMS[chip],
        System::Atsc => &ATSC_PARAMS[chip],
        System::Fm => &[],
    }
}

fn freq_params(system: System) -> &'static [FreqParams] {
    match system {
        System::DvbT | System::DvbT2 | System::DvbT2_1 => &DVB_T_T2_FREQ_PARAMS,
        System::DvbC => &DVB_C_FREQ_PARAMS,
        System::J83b => &J83B_FREQ_PARAMS,
        System::IsdbT => &ISDB_T_FREQ_PARAMS,
        System::Dtmb => &DTMB_FREQ_PARAMS,
        System::Atsc => &ATSC_FREQ_PARAMS,
        System::Fm => &[],
    }
}

/// The fractional part of the PLL divider, found bit by bit.
fn pll_sdm(mut vco_fra: u16, xtal: u32) -> u16 {
    let mut nsdm: u32 = 2;
    let mut sdm: u32 = 0;
    // C lets nsdm (u16) overflow to 0 and divides by it; with a 24 MHz
    // crystal it always breaks at 0x8000 first, so stop there.
    while vco_fra > 1 && nsdm <= 0x8000 {
        if xtal * 2 / nsdm < u32::from(vco_fra) {
            vco_fra -= (xtal * 2 / nsdm) as u16;
            sdm += 0x8000 / (nsdm / 2);
            if nsdm & 0x8000 != 0 {
                break;
            }
        }
        nsdm += nsdm;
    }
    sdm as u16
}

/// The value of the first step whose bound `freq` is below, else `last`.
fn below<T: Copy>(freq: u32, steps: &[(u32, T)], last: T) -> T {
    steps
        .iter()
        .find(|&&(bound, _)| freq < bound)
        .map_or(last, |&(_, v)| v)
}

async fn sleep_ms(ms: u64) {
    Delay::new(Duration::from_millis(ms)).await;
}

fn not_initialized() -> tunelith_core::Error {
    error("R850 is not initialised")
}

pub struct R850 {
    i2c_addr: u8,
    config: R850Config,
    init: bool,
    /// 0 or 1, from the id register; picks the table row set.
    chip: usize,
    xtal_pwr: u8,
    regs: [u8; NUM_REGS],
    sys: Option<SystemConfig>,
    /// 0 or 1.
    mixer_mode: usize,
    mixer_amp_lpf_imr_cal: u8,
    imr_cal: [ImrCal; 2],
    sys_curr: Option<SystemConfig>,
}

impl R850 {
    pub fn new(i2c_addr: u8, config: R850Config) -> Self {
        Self {
            i2c_addr,
            config,
            init: false,
            chip: 0,
            xtal_pwr: 0,
            regs: [0; NUM_REGS],
            sys: None,
            mixer_mode: 0,
            mixer_amp_lpf_imr_cal: 0,
            imr_cal: [ImrCal::default(); 2],
            sys_curr: None,
        }
    }

    /// Reads `buf.len()` registers from `reg`. The chip always sends from
    /// register 0 on, with the bits of each byte reversed.
    async fn read_regs(&self, i2c: &mut impl I2c, reg: usize, buf: &mut [u8]) -> Result<()> {
        let mut b = [0; NUM_REGS];
        let b = &mut b[..reg + buf.len()];
        i2c.write_read(self.i2c_addr, &[0x00], b).await?;
        for (d, s) in buf.iter_mut().zip(&b[reg..]) {
            *d = s.reverse_bits();
        }
        Ok(())
    }

    async fn read_reg(&self, i2c: &mut impl I2c, reg: usize) -> Result<u8> {
        let mut b = [0];
        self.read_regs(i2c, reg, &mut b).await?;
        Ok(b[0])
    }

    async fn write_regs(&self, i2c: &mut impl I2c, reg: usize, data: &[u8]) -> Result<()> {
        let mut b = Vec::with_capacity(1 + data.len());
        b.push(reg as u8);
        b.extend_from_slice(data);
        i2c.write(self.i2c_addr, &b).await
    }

    /// Writes `len` registers from `reg` out of the shadow.
    async fn flush(&self, i2c: &mut impl I2c, reg: usize, len: usize) -> Result<()> {
        self.write_regs(i2c, reg, &self.regs[reg..reg + len]).await
    }

    fn init_regs(&mut self) {
        self.regs = INIT_REGS;
    }

    fn set_xtal_cap(&mut self, cap: u8) {
        let (c, g) = if cap > 0x1f {
            (cap - 10, 0x80)
        } else {
            (cap, 0x00)
        };
        let r = &mut self.regs;
        r[0x21] = (r[0x21] & 0x07) | ((c << 2) & 0x78) | g;
        r[0x22] = (r[0x22] & 0xf7) | ((c << 3) & 0x08);
    }

    async fn set_pll(
        &mut self,
        i2c: &mut impl I2c,
        lo_freq: u32,
        if_freq: u32,
        sys: Option<System>,
    ) -> Result<()> {
        let mut xtal = self.config.xtal;
        let chip = self.chip != 0;

        let mut vco_min: u32 = 2_200_000;
        if !chip {
            vco_min += 70_000;
        }
        let vco_max = vco_min * 2;
        let mut mix_div: u32 = 2;
        let mut vco_freq = lo_freq * mix_div;

        let r = &mut self.regs;
        r[0x20] &= 0xfc;
        r[0x2e] |= 0x40;
        r[0x0c] &= 0x3c;
        r[0x09] &= 0xf9;
        r[0x22] &= 0x3f;
        r[0x0b] &= 0xc3;
        r[0x0b] |= 0x10;
        r[0x25] &= 0xef;
        r[0x25] |= 0x20;

        let pwr = self.xtal_pwr;
        let b: u8 = if lo_freq < 100_000 {
            if pwr > 1 { 3 - pwr } else { 2 }
        } else if lo_freq < 130_000 {
            if pwr > 2 { 3 - pwr } else { 1 }
        } else {
            0
        };

        self.set_xtal_cap(0x27);

        let r = &mut self.regs;
        r[0x22] &= 0xcf;
        r[0x22] |= (b << 4) & 0x30;

        // Assumes a 24 MHz crystal, as C does.
        let div_judge = (lo_freq + if_freq) / 1000 / 12;

        r[0x1e] &= 0x1f;
        r[0x25] &= 0xfd;
        if matches!(div_judge, 4 | 10 | 22 | 24 | 28) {
            r[0x25] |= 0x02;
        }

        r[0x2f] &= if chip { 0xfd } else { 0xfc };

        let mut div: u8 = 0;
        while div < 6 {
            if vco_min <= vco_freq && vco_freq < vco_max {
                break;
            }
            mix_div *= 2;
            vco_freq = lo_freq * mix_div;
            div += 1;
        }

        let mut xtal_div = 0;
        r[0x22] &= 0xfc;
        if let Some(sys) = sys {
            if lo_freq < 380_500 {
                if div_judge & 1 == 0 {
                    xtal /= 2;
                    r[0x22] |= 0x02;
                    xtal_div = 1;
                }
            } else if (lo_freq + if_freq).wrapping_sub(478_000) < 4000 && sys == System::IsdbT {
                xtal /= 4;
                r[0x22] |= 0x03;
                xtal_div = 3;
            }
        }

        r[0x0b] &= 0xfe;

        r[0x2d] &= 0xf3;
        match mix_div {
            8 => r[0x2d] |= 0x04,
            16 => r[0x2d] |= 0x08,
            32.. => r[0x2d] |= 0x0c,
            _ => {}
        }

        r[0x2e] &= 0xfc;
        r[0x20] &= 0xec;
        if mix_div == 2 || mix_div == 4 {
            r[0x2e] |= 0x01;
        } else {
            r[0x2e] |= 0x02;
            r[0x20] |= 0x01;
        }

        r[0x11] &= 0x7f;
        if mix_div == 8 {
            r[0x11] |= 0x80;
        }

        r[0x1e] &= 0xe3;
        r[0x1e] |= (div << 2) & 0x1c;

        let mut nint = ((vco_freq / 2) / xtal) as u16;
        let mut vco_fra = (vco_freq - xtal * 2 * u32::from(nint)) as u16;
        let fra = u32::from(vco_fra);
        if fra < xtal / 64 {
            vco_fra = 0;
        } else if fra > xtal * 127 / 64 {
            vco_fra = 0;
            nint += 1;
        } else if fra > xtal * 127 / 128 && xtal > fra {
            vco_fra = (xtal * 127 / 128) as u16;
        } else if xtal < fra && fra < xtal * 129 / 128 {
            vco_fra = (xtal * 129 / 128) as u16;
        }

        let ni = ((i32::from(nint) - 13) / 4) as u8;
        let si = (i32::from(nint) - 13 - i32::from(ni) * 4) as u8;

        r[0x1b] &= 0x80;
        r[0x1b] |= ni & 0x7f;
        r[0x1e] &= 0xfc;
        r[0x1e] |= si & 0x03;
        r[0x20] &= 0x3f;

        let [sdm_lo, sdm_hi] = pll_sdm(vco_fra, xtal).to_le_bytes();
        r[0x1c] = sdm_lo;
        r[0x1d] = sdm_hi;

        self.flush(i2c, 0x08, 0x28).await?;

        sleep_ms(match xtal_div {
            0 => 10,
            1 | 2 => 20,
            _ => 40,
        })
        .await;

        if !chip {
            self.regs[0x2f] &= 0xfc;
        }
        self.regs[0x2f] |= 0x02;
        self.flush(i2c, 0x2f, 1).await
    }

    fn set_mux(&mut self, _rf_freq: u32, lo_freq: u32, sys: Option<System>) {
        let imr_idx = below(
            lo_freq,
            &[(170_000, 0), (240_000, 4), (400_000, 1), (760_000, 2)],
            3,
        );
        let tf_hpf_bpf: u8 = below(
            lo_freq,
            &[(580_000, 7), (660_000, 1), (780_000, 6), (900_000, 4)],
            0,
        );
        let rf_poly: u8 = below(lo_freq, &[(133_000, 2), (221_000, 1), (760_000, 0)], 3);
        let tf_hpf_cnr: u8 = below(lo_freq, &[(480_000, 3), (550_000, 2), (700_000, 1)], 0);
        let (lpf_notch, lpf_cap): (u8, u8) = if matches!(sys, Some(System::DvbC | System::J83b)) {
            below(
                lo_freq,
                &[
                    (77_000, (10, 15)),
                    (85_000, (4, 15)),
                    (115_000, (3, 13)),
                    (125_000, (1, 11)),
                    (141_000, (0, 9)),
                    (157_000, (0, 8)),
                    (181_000, (0, 6)),
                    (205_000, (0, 3)),
                ],
                (0, 0),
            )
        } else {
            below(
                lo_freq,
                &[
                    (73_000, (10, 8)),
                    (81_000, (4, 8)),
                    (89_000, (3, 8)),
                    (121_000, (1, 6)),
                    (145_000, (0, 4)),
                    (153_000, (0, 3)),
                    (177_000, (0, 2)),
                    (201_000, (0, 1)),
                ],
                (0, 0),
            )
        };
        let tf_diplexer: u8 = if lo_freq < 330_000 { 2 } else { 0 };

        let cal = &self.imr_cal[self.mixer_mode];
        let (imr_gain, imr_phase, imr_iqcap) = if cal.done && cal.result[imr_idx] {
            let imr = &cal.imr[imr_idx];
            (imr.gain, imr.phase, imr.iqcap)
        } else if sys.is_some() {
            (0x02, 0x00, 0x00)
        } else {
            (0x00, 0x00, 0x00)
        };

        let r = &mut self.regs;
        r[0x0e] &= 0x03;
        r[0x0e] |= (tf_diplexer << 2) & 0x0c;
        r[0x0e] |= (lpf_cap << 4) & 0xf0;

        r[0x0f] &= 0xf0;
        r[0x0f] |= lpf_notch & 0x0f;

        r[0x10] &= 0xe0;
        r[0x10] |= (tf_hpf_cnr << 3) & 0x18;
        r[0x10] |= tf_hpf_bpf & 0x07;

        r[0x12] &= 0xfc;
        r[0x12] |= rf_poly & 0x03;

        r[0x14] &= 0xd0;
        r[0x14] |= imr_gain & 0x2f;

        r[0x15] &= 0x10;
        r[0x15] |= imr_phase & 0x2f;
        r[0x15] |= (imr_iqcap << 6) & 0xc0;
    }

    async fn read_adc_value(&self, i2c: &mut impl I2c) -> Result<u8> {
        sleep_ms(2).await;
        Ok(self.read_reg(i2c, 0x01).await? & 0x3f)
    }

    /// Sets the IMR gain and phase (without the iqcap bits) and reads the ADC.
    async fn imr_try(&mut self, i2c: &mut impl I2c) -> Result<u8> {
        self.flush(i2c, 0x14, 2).await?;
        self.read_adc_value(i2c).await
    }

    async fn imr_check_iq_cross(&mut self, i2c: &mut impl I2c) -> Result<(Imr, Direction)> {
        // (gain, phase)
        const CROSS: [(u8, u8); 9] = [
            (0, 0),
            (0, 1),
            (0, 0x20 | 1),
            (1, 0),
            (0x20 | 1, 0),
            (0, 2),
            (0, 0x20 | 2),
            (2, 0),
            (0x20 | 2, 0),
        ];

        let mut best = Imr {
            value: 0xff,
            ..Imr::default()
        };
        for (gain, phase) in CROSS {
            self.regs[0x14] &= 0xd0;
            self.regs[0x14] |= gain & 0x2f;
            self.regs[0x15] &= 0xd0;
            self.regs[0x15] |= phase & 0x2f;

            let tmp = self.imr_try(i2c).await?;
            if best.value > tmp {
                best.gain = gain;
                best.phase = phase;
                best.value = tmp;
            }
        }

        let direction = if best.phase != 0 {
            Direction::Phase
        } else {
            Direction::Gain
        };
        Ok((best, direction))
    }

    /// Fixes the other axis of `imr` and returns the register and the
    /// current value along `direction`.
    fn imr_fix_other(&mut self, imr: &Imr, direction: Direction) -> (usize, u8) {
        match direction {
            Direction::Gain => {
                self.regs[0x15] &= 0xd0;
                self.regs[0x15] |= imr.phase & 0x2f;
                (0x14, imr.gain)
            }
            Direction::Phase => {
                self.regs[0x14] &= 0xd0;
                self.regs[0x14] |= imr.gain & 0x2f;
                (0x15, imr.phase)
            }
        }
    }

    async fn imr_check_iq_tree(
        &mut self,
        i2c: &mut impl I2c,
        imr: &mut Imr,
        direction: Direction,
        num: usize,
    ) -> Result<()> {
        let (reg, v0) = self.imr_fix_other(imr, direction);

        let mut val = [v0, v0.wrapping_add(1), 0, 0, 0];
        match num {
            3 => {
                val[2] = if v0 & 0x0f == 0 {
                    (v0 ^ 0x20).wrapping_add(1)
                } else {
                    v0.wrapping_sub(1)
                };
            }
            5 => {
                val[2] = v0.wrapping_add(2);
                match v0 & 0x0f {
                    0 => {
                        val[3] = (v0 ^ 0x20).wrapping_add(1);
                        val[4] = val[3].wrapping_add(1);
                    }
                    1 => {
                        val[3] = v0.wrapping_sub(1);
                        val[4] = (val[3] ^ 0x20).wrapping_add(1);
                    }
                    _ => {
                        val[3] = v0.wrapping_sub(1);
                        val[4] = val[3].wrapping_sub(1);
                    }
                }
            }
            _ => return Err(error("R850: invalid IMR tree size")),
        }

        let mut best = Imr {
            value: 0xff,
            ..*imr
        };
        for &v in &val[..num] {
            self.regs[reg] &= 0xd0;
            self.regs[reg] |= v & 0x2f;

            let tmp = self.imr_try(i2c).await?;
            if best.value > tmp {
                match direction {
                    Direction::Gain => best.gain = v,
                    Direction::Phase => best.phase = v,
                }
                best.value = tmp;
            }
        }

        *imr = best;
        Ok(())
    }

    async fn imr_check_iq_step(
        &mut self,
        i2c: &mut impl I2c,
        imr: &mut Imr,
        direction: Direction,
    ) -> Result<()> {
        let (reg, mut val) = self.imr_fix_other(imr, direction);
        let mut best = *imr;

        while val & 0x0f <= 8 {
            val = val.wrapping_add(1);
            self.regs[reg] &= 0xd0;
            self.regs[reg] |= val & 0x2f;

            let tmp = self.imr_try(i2c).await?;
            if best.value > tmp {
                match direction {
                    Direction::Gain => best.gain = val,
                    Direction::Phase => best.phase = val,
                }
                best.value = tmp;
            } else if u16::from(best.value) + 2 < u16::from(tmp) {
                break;
            }
        }

        *imr = best;
        Ok(())
    }

    async fn imr_check_section(&mut self, i2c: &mut impl I2c, imr: &mut Imr) -> Result<()> {
        let (g0, g2) = if imr.gain != 0 {
            (imr.gain - 1, imr.gain + 1)
        } else {
            ((imr.gain & 0xdf) + 1, (imr.gain | 0x20) + 1)
        };
        let mut points = [g0, imr.gain, g2].map(|gain| Imr {
            gain,
            phase: imr.phase,
            ..Imr::default()
        });

        let mut val = 0xff;
        let mut n = 0;
        for (i, point) in points.iter_mut().enumerate() {
            self.imr_check_iq_tree(i2c, point, Direction::Phase, 3)
                .await?;
            if val > point.value {
                val = point.value;
                n = i;
            }
        }

        *imr = points[n];
        Ok(())
    }

    async fn imr_check_iqcap(&mut self, i2c: &mut impl I2c, imr: &mut Imr) -> Result<()> {
        self.regs[0x14] &= 0xd0;
        self.regs[0x14] |= imr.gain & 0x2f;
        self.flush(i2c, 0x14, 1).await?;

        self.regs[0x15] &= 0xd0;
        self.regs[0x15] |= imr.phase & 0x2f;

        imr.iqcap = 0;
        imr.value = 0xff;

        for i in 0..3u8 {
            self.regs[0x15] &= 0x3f;
            self.regs[0x15] |= (i << 6) & 0xc0;
            self.flush(i2c, 0x15, 1).await?;

            let tmp = self.read_adc_value(i2c).await?;
            if tmp < imr.value {
                imr.iqcap = i;
                imr.value = tmp;
            }
        }
        Ok(())
    }

    fn prepare_calibration(&mut self, regs: &[u8; NUM_REGS]) {
        // C does not write them out here either.
        self.regs = *regs;
    }

    async fn calibrate_imr(&mut self, i2c: &mut impl I2c) -> Result<()> {
        let mixer_mode = self.mixer_mode;
        let mixer_amp_lpf = self.mixer_amp_lpf_imr_cal;

        for j in [2, 1, 0, 3, 4] {
            let mut full = false;
            let mut pre = 2;

            self.regs[0x24] &= 0xf0;
            let ring_freq: u32 = match j {
                0 => {
                    self.regs[0x24] |= 0x0a;
                    pre = 1;
                    136_000
                }
                1 => {
                    self.regs[0x24] |= 0x05;
                    326_400
                }
                2 => {
                    self.regs[0x24] |= 0x02;
                    full = true;
                    544_000
                }
                3 => {
                    if mixer_mode != 0 {
                        full = true;
                    }
                    816_000
                }
                _ => {
                    self.regs[0x24] |= 0x08;
                    pre = 1;
                    204_000
                }
            };

            self.regs[0x23] &= 0xa0;
            self.regs[0x23] |= 0x11;

            if mixer_mode == 0 {
                self.set_mux(ring_freq - 5300, ring_freq, None);
                self.set_pll(i2c, ring_freq - 5300, 5300, None).await?;

                self.regs[0x13] &= 0xe8;
                self.regs[0x13] |= mixer_amp_lpf & 0x07;
                self.flush(i2c, 0x13, 1).await?;

                if j == 4 {
                    self.regs[0x24] &= 0xcf;
                    self.regs[0x24] |= 0x10;
                } else {
                    self.regs[0x24] |= 0x30;
                }
                self.flush(i2c, 0x24, 1).await?;

                self.regs[0x29] &= 0xf0;
                self.regs[0x29] |= 0x08;
                self.flush(i2c, 0x29, 1).await?;
            } else {
                self.set_mux(ring_freq + 5300, ring_freq, None);
                self.set_pll(i2c, ring_freq + 5300, 5300, None).await?;

                self.regs[0x13] |= 0x10;
                self.regs[0x13] &= 0xf8;
                self.regs[0x13] |= mixer_amp_lpf & 0x07;
                self.flush(i2c, 0x13, 1).await?;

                self.regs[0x29] &= 0xf0;
                if j == 4 {
                    self.regs[0x29] |= 0x07;
                    self.regs[0x24] &= 0xcf;
                    self.regs[0x24] |= 0x10;
                } else {
                    self.regs[0x29] |= 0x06;
                    self.regs[0x24] |= 0x30;
                }
                self.flush(i2c, 0x29, 1).await?;
                self.flush(i2c, 0x24, 1).await?;
            }

            self.regs[0x29] |= 0xf0;
            self.flush(i2c, 0x29, 1).await?;

            let mut imr = if full {
                let (mut imr, d) = self.imr_check_iq_cross(i2c).await?;
                self.imr_check_iq_step(i2c, &mut imr, d).await?;
                let other = match d {
                    Direction::Gain => Direction::Phase,
                    Direction::Phase => Direction::Gain,
                };
                self.imr_check_iq_tree(i2c, &mut imr, other, 5).await?;
                self.imr_check_iq_tree(i2c, &mut imr, d, 3).await?;
                imr
            } else {
                self.imr_cal[mixer_mode].imr[pre]
            };

            self.imr_check_section(i2c, &mut imr).await?;
            self.imr_check_iqcap(i2c, &mut imr).await?;

            let cal = &mut self.imr_cal[mixer_mode];
            cal.imr[j] = imr;
            cal.result[j] = imr.gain & 0x0f <= 0x06 && imr.phase & 0x0f <= 0x06;

            if full {
                // Resets gain, phase and iqcap.
                self.regs[0x14] &= 0xd0;
                self.regs[0x15] &= 0x10;
                self.flush(i2c, 0x14, 2).await?;
            }
        }

        let cal = &mut self.imr_cal[mixer_mode];
        cal.done = true;
        cal.mixer_amp_lpf = mixer_amp_lpf;
        Ok(())
    }

    async fn calibrate_lpf(
        &mut self,
        i2c: &mut impl I2c,
        if_freq: u32,
        bw: u8,
        gap: u8,
    ) -> Result<Lpf> {
        let gap = u16::from(gap);

        self.set_pll(i2c, 72_000 - if_freq, if_freq, None).await?;

        let mut val = 0;
        for i in 5..16u8 {
            self.regs[0x29] &= 0x0f;
            self.regs[0x29] |= (i << 4) & 0xf0;
            self.flush(i2c, 0x29, 1).await?;
            sleep_ms(5).await;
            val = self.read_adc_value(i2c).await?;
            if val > 0x28 {
                break;
            }
        }

        let mut val3 = 0;
        if if_freq > 9999 {
            self.set_pll(i2c, 63_500, 8500, None).await?;
            sleep_ms(5).await;
            val3 = self.read_adc_value(i2c).await?;
            if u16::from(val3) <= u16::from(val) + 8 {
                self.set_pll(i2c, 72_000 - if_freq, if_freq, None).await?;
            } else {
                return Err(error("R850: LPF calibration failed"));
            }
        }

        let mut bandwidth = 0;
        for i in if bw == 2 { 1 } else { 0 }..3u8 {
            bandwidth = if i == 0 { 0 } else { i + 1 };

            self.regs[0x17] &= 0x9f;
            self.regs[0x17] &= 0xe1;
            self.regs[0x17] |= (bandwidth << 5) & 0x60;
            self.flush(i2c, 0x17, 1).await?;
            sleep_ms(5).await;
            val = self.read_adc_value(i2c).await?;

            self.regs[0x17] &= 0xe1;
            self.regs[0x17] |= 0x1a;
            self.flush(i2c, 0x17, 1).await?;
            sleep_ms(5).await;
            let val2 = self.read_adc_value(i2c).await?;

            if u16::from(val2) + 16 < u16::from(val) {
                break;
            }
        }

        let mut lpf = Lpf {
            code: 16,
            bandwidth,
            lsb: 0,
        };

        for i in 0..16u8 {
            self.regs[0x17] &= 0xe1;
            self.regs[0x17] |= (i << 1) & 0x1e;
            self.flush(i2c, 0x17, 1).await?;
            sleep_ms(5).await;
            let val2 = self.read_adc_value(i2c).await?;

            if i == 0 {
                val = if if_freq <= 9999 { val2 } else { val3 };
            }

            if u16::from(val2) + gap < u16::from(val) {
                if i == 0 {
                    return Err(error("R850: LPF calibration failed"));
                }

                self.regs[0x17] &= 0xe0;
                self.regs[0x17] |= 1 | (((i - 1) << 1) & 0x1e);
                self.flush(i2c, 0x17, 1).await?;
                sleep_ms(5).await;
                let val2 = self.read_adc_value(i2c).await?;

                lpf.code = i;
                if u16::from(val2) + gap < u16::from(val) {
                    lpf.code = i - 1;
                    lpf.lsb = 1;
                }
                break;
            }
        }

        Ok(lpf)
    }

    async fn set_system_params(&mut self, i2c: &mut impl I2c) -> Result<SystemConfig> {
        let sys = self.sys.ok_or_else(|| error("R850: no system is set"))?;

        let cal = &self.imr_cal[self.mixer_mode];
        if !self.config.no_imr_calibration
            && (!cal.done || cal.mixer_amp_lpf != self.mixer_amp_lpf_imr_cal)
        {
            self.prepare_calibration(&IMR_CAL_REGS);
            self.calibrate_imr(i2c).await?;
        }

        if self.sys_curr != Some(sys) {
            let prm = *system_params(sys.system, self.chip)
                .iter()
                .find(|p| p.bandwidth == sys.bandwidth && p.if_freq == sys.if_freq)
                .ok_or_else(|| error("R850: unsupported system"))?;

            let lpf = if !self.config.no_lpf_calibration {
                self.prepare_calibration(&LPF_CAL_REGS);
                self.calibrate_lpf(i2c, prm.filt_cal_if, prm.bw, 2).await?
            } else {
                prm.lpf
            };

            self.init_regs();

            let r = &mut self.regs;
            r[0x17] = 0x00;
            r[0x17] |= lpf.lsb & 0x01;
            r[0x17] |= (lpf.code << 1) & 0x1e;
            r[0x17] |= (lpf.bandwidth << 5) & 0x60;
            r[0x17] |= (prm.hpf_notch << 7) & 0x80;

            r[0x18] &= 0x0f;
            r[0x18] |= (prm.hpf_cor << 4) & 0xf0;

            r[0x12] &= 0xbf;
            r[0x12] |= (prm.filt_ext_ena << 6) & 0x40;

            r[0x18] &= 0xf3;
            r[0x18] |= (prm.filt_comp << 2) & 0x0c;

            r[0x2f] &= 0xf3;
            r[0x2f] |= (prm.agc_clk << 2) & 0x0c;

            if self.chip != 0 {
                r[0x2c] &= 0xfe;
                r[0x2c] |= (prm.img_gain >> 1) & 0x01;
            }

            r[0x2e] &= 0xef;
            r[0x2e] |= (prm.img_gain << 4) & 0x10;

            self.sys_curr = Some(sys);
        }

        Ok(sys)
    }

    async fn set_system_frequency(
        &mut self,
        i2c: &mut impl I2c,
        sys: SystemConfig,
        rf_freq: u32,
    ) -> Result<()> {
        let mut prm = *freq_params(sys.system)
            .iter()
            .find(|p| {
                (p.if_freq == 0 || p.if_freq == sys.if_freq)
                    && (p.rf_freq_min == 0 || p.rf_freq_min <= rf_freq)
                    && (p.rf_freq_max == 0 || p.rf_freq_max >= rf_freq)
            })
            .ok_or_else(|| error("R850: unsupported frequency"))?;

        if matches!(sys.system, System::DvbC | System::J83b | System::IsdbT) && self.chip != 0 {
            prm.filter_top = 6;
        }

        let chip = self.chip != 0;
        let r = &mut self.regs;

        r[0x13] &= 0xef;
        let lo_freq = if self.mixer_mode != 0 {
            r[0x13] |= 0x10;
            rf_freq - sys.if_freq
        } else {
            rf_freq + sys.if_freq
        };

        r[0x0a] &= 0xbf;
        r[0x0a] |= (prm.na_pwr_det << 6) & 0x40;

        // As in C, which reads register 0x0c here, a no-op with these tables.
        r[0x10] &= 0xdf;
        r[0x10] |= INIT_REGS[0x0c] & 0x20;

        r[0x0b] &= 0x7f;
        r[0x0b] |= (prm.lna_nrb_det << 7) & 0x80;

        r[0x26] &= 0xf8;
        r[0x26] |= (7 - prm.lna_top) & 0x07;

        r[0x27] = prm.lna_vtl_h;

        r[0x11] &= 0xef;
        r[0x11] |= (prm.rf_lte_psg << 4) & 0x10;

        r[0x26] &= 0x8f;
        r[0x26] |= ((7 - prm.rf_top) << 4) & 0x70;

        r[0x2a] = prm.rf_vtl_h;

        if prm.rf_gain_limit <= 3 {
            // C clears 0x04 but sets 0x02; kept as is.
            if prm.rf_gain_limit < 2 {
                r[0x12] &= 0xfb;
            } else {
                r[0x12] |= 0x02;
            }

            if prm.rf_gain_limit % 2 != 0 {
                r[0x10] |= 0x40;
            } else {
                r[0x10] &= 0xbf;
            }
        }

        r[0x13] &= 0xf8;
        r[0x13] |= prm.mixer_amp_lpf & 0x07;

        r[0x28] &= 0xf0;
        r[0x28] |= (15 - prm.mixer_top) & 0x0f;

        if chip {
            r[0x2c] &= 0xf1;
            r[0x2c] |= ((7 - prm.filter_top) << 1) & 0x0e;
        } else {
            r[0x2c] &= 0xf0;
            r[0x2c] |= (15 - prm.filter_top) & 0x0f;
        }

        r[0x0a] &= 0xef;
        r[0x0a] |= (prm.filt_3th_lpf_cur << 4) & 0x10;

        r[0x18] &= 0xfc;
        r[0x18] |= prm.filt_3th_lpf_gain & 0x03;

        r[0x29] = ((prm.filter_vth << 4) & 0xf0) | (prm.mixer_vth & 0x0f);
        r[0x2b] = ((prm.filter_vtl << 4) & 0xf0) | (prm.mixer_vtl & 0x0f);

        r[0x16] &= 0x3f;
        r[0x16] |= (prm.mixer_gain_limit << 6) & 0xc0;

        r[0x2e] &= 0x7f;
        r[0x2e] |= (prm.mixer_detbw_lpf << 7) & 0x80;

        match prm.lna_rf_dis_mode {
            1 => {
                r[0x2d] |= 0x03;
                r[0x1f] |= 0x01;
                r[0x20] |= 0x20;
            }
            2 => {
                r[0x2d] |= 0x03;
                r[0x1f] &= 0xfe;
                r[0x20] &= 0xdf;
            }
            3 => {
                r[0x2d] |= 0x03;
                r[0x1f] |= 0x01;
                r[0x20] &= 0xdf;
            }
            4 => {
                r[0x2d] |= 0x03;
                r[0x1f] &= 0xfe;
                r[0x20] |= 0x20;
            }
            _ => {
                r[0x2d] &= 0xfc;
                r[0x1f] |= 0x01;
                r[0x20] |= 0x20;
            }
        }

        r[0x1f] &= 0xfd;
        r[0x1f] |= (prm.lna_rf_charge_cur << 1) & 0x02;

        r[0x0d] &= 0xdf;
        r[0x0d] |= (prm.lna_rf_dis_curr << 5) & 0x20;

        r[0x2d] &= 0x0f;
        r[0x2d] |= (prm.rf_dis_slow_fast << 4) & 0xf0;

        r[0x2c] &= 0x0f;
        r[0x2c] |= (prm.lna_dis_slow_fast << 4) & 0xf0;

        r[0x19] &= 0xbf;
        r[0x19] |= (prm.bb_dis_curr << 6) & 0x40;

        r[0x25] &= 0x3b;
        r[0x25] |= ((prm.mixer_filter_dis << 6) & 0xc0) | ((prm.bb_det_mode << 2) & 0x04);

        r[0x19] &= 0xfd;
        r[0x19] |= (prm.enb_poly_gain << 1) & 0x02;

        r[0x28] &= 0x0f;
        r[0x28] |= ((15 - prm.nrb_top) << 4) & 0xf0;

        r[0x1a] &= 0x33;
        r[0x1a] |= ((prm.nrb_bw_lpf << 6) & 0xc0) | ((prm.nrb_bw_hpf << 2) & 0x0c);

        r[0x2e] &= 0xf3;
        r[0x2e] |= (prm.img_nrb_adder << 2) & 0x0c;

        r[0x0d] &= 0xf9;
        r[0x0d] |= (prm.hpf_comp << 1) & 0x06;

        r[0x15] &= 0xef;
        r[0x15] |= (prm.fb_res_1st << 4) & 0x10;

        if rf_freq.wrapping_sub(478_000) <= 3999 && sys.system == System::IsdbT {
            r[0x2f] &= 0xf3;
        }

        r[0x19] &= 0xdf;

        if self.config.loop_through {
            r[0x08] |= 0xc0;
            r[0x0a] |= 0x02;
        } else {
            r[0x08] &= 0x3f;
            r[0x08] |= 0x40;
            r[0x0a] &= 0xfd;
        }

        if self.config.clock_out {
            r[0x22] &= 0xfb;
        } else {
            r[0x22] |= 0x04;
        }

        self.set_mux(rf_freq, lo_freq, Some(sys.system));
        self.set_pll(i2c, lo_freq, sys.if_freq, Some(sys.system))
            .await
    }

    async fn check_xtal_power(&mut self, i2c: &mut impl I2c) -> Result<()> {
        // For a 24 MHz crystal.
        let bank: i32 = 55;
        let mut pwr: u8 = 3;

        self.init_regs();
        let r = &mut self.regs;

        r[0x2f] &= if self.chip != 0 { 0xfd } else { 0xfc };
        r[0x1b] &= 0x80;
        r[0x1b] |= 0x12;
        r[0x1e] &= 0xe0;
        r[0x1e] |= 0x08;
        r[0x22] &= 0x27;
        r[0x1d] &= 0x0f;
        r[0x21] |= 0xf8;
        r[0x22] &= 0x77;
        r[0x22] |= 0x80;
        r[0x1f] &= 0x80;
        r[0x1f] |= 0x40;
        r[0x1f] &= 0xbf;

        self.flush(i2c, 0x08, NUM_REGS - 0x08).await?;

        for i in 0..=3u8 {
            self.regs[0x22] &= 0xcf;
            self.regs[0x22] |= i << 4;
            self.flush(i2c, 0x22, 1).await?;

            let tmp = self.read_reg(i2c, 0x02).await?;
            // Signed in C, so any bank below the window passes too.
            if tmp & 0x40 != 0 && i32::from(tmp & 0x3f) - (bank - 6) <= 12 {
                pwr = i;
                break;
            }
        }

        if pwr < 3 {
            pwr += 1;
        }
        self.xtal_pwr = pwr;
        Ok(())
    }

    pub async fn init(&mut self, i2c: &mut impl I2c) -> Result<()> {
        self.init = false;
        self.chip = 0;
        self.sys = None;
        self.imr_cal[0].done = false;
        self.imr_cal[1].done = false;
        self.sys_curr = None;

        // Only the last attempt's error counts, as in C.
        let mut ret = Ok(());
        for _ in 0..4 {
            ret = self.read_reg(i2c, 0x00).await.map(|tmp| {
                if tmp & 0x98 != 0 {
                    self.chip = 1;
                }
            });
            if ret.is_ok() && self.chip != 0 {
                break;
            }
        }
        ret?;

        let mut regs = [0; NUM_REGS];
        self.read_regs(i2c, 0x08, &mut regs[0x08..]).await?;

        self.check_xtal_power(i2c).await?;

        self.write_regs(i2c, 0x08, &regs[0x08..]).await?;

        self.init_regs();
        self.init = true;
        Ok(())
    }

    pub fn term(&mut self) {
        if !self.init {
            return;
        }
        self.sys = None;
        self.imr_cal[0].done = false;
        self.imr_cal[1].done = false;
        self.sys_curr = None;
        self.regs = [0; NUM_REGS];
        self.chip = 0;
        self.init = false;
    }

    /// Does nothing but check, as the sleep sequence is disabled in C.
    pub fn sleep(&mut self) -> Result<()> {
        if !self.init {
            return Err(not_initialized());
        }
        Ok(())
    }

    /// Does nothing but check, as the wakeup sequence is disabled in C.
    pub fn wakeup(&mut self) -> Result<()> {
        if !self.init {
            return Err(not_initialized());
        }
        Ok(())
    }

    pub fn set_system(&mut self, system: SystemConfig) -> Result<()> {
        if !self.init {
            return Err(not_initialized());
        }

        let (mixer_mode, mixer_amp_lpf_imr_cal) = match system.system {
            System::DvbT | System::DvbT2 | System::DvbT2_1 | System::DvbC | System::Fm => (1, 4),
            System::J83b | System::Dtmb | System::Atsc => (0, 7),
            System::IsdbT => (1, 7),
        };

        self.sys = Some(system);
        self.mixer_mode = mixer_mode;
        self.mixer_amp_lpf_imr_cal = mixer_amp_lpf_imr_cal;
        self.sys_curr = None;
        Ok(())
    }

    /// Tunes to `freq` in kHz, in the system last set.
    pub async fn set_frequency(&mut self, i2c: &mut impl I2c, freq: u32) -> Result<()> {
        if !self.init {
            return Err(not_initialized());
        }
        if !(40_000..=1_002_000).contains(&freq) {
            return Err(error("R850: frequency out of range"));
        }

        let sys = self.set_system_params(i2c).await?;
        self.set_system_frequency(i2c, sys, freq).await
    }

    pub async fn is_pll_locked(&mut self, i2c: &mut impl I2c) -> Result<bool> {
        if !self.init {
            return Err(not_initialized());
        }
        Ok(self.read_reg(i2c, 0x02).await? & 0x40 != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdm_matches_c() {
        // Computed by the C loop of `r850_set_pll`.
        for (fra, xtal, sdm) in [
            (0, 24000, 0x0000),
            (1, 24000, 0x0000),
            (36000, 24000, 0xbffe),
            (24000, 24000, 0x7ffe),
            (12345, 24000, 0x41dc),
            (47000, 24000, 0xfaae),
            (30311, 24000, 0xa1ac),
            (12345, 12000, 0x83b8),
        ] {
            assert_eq!(pll_sdm(fra, xtal), sdm, "fra {fra}, xtal {xtal}");
        }
    }
}
