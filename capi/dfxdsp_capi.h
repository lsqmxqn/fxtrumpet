/*
 * FxTrumpet — C ABI wrapper around FxSound's DfxDsp engine.
 *
 * Upstream (fxsound-app/dsp/include/DfxDsp.h) is a C++ class whose methods take
 * std::wstring by value, so it cannot be bound from Rust directly. This shim
 * exposes a flat, opaque-handle C surface that stays stable across compilers.
 *
 * Derived from FxSound, Copyright (C) 2025 FxSound LLC.
 * Licensed under the GNU Affero General Public License v3.0, same as upstream.
 *
 * ---------------------------------------------------------------------------
 * Two upstream quirks this header deliberately normalises:
 *
 *  1. Return codes are INVERTED upstream: `#define OKAY 0` (codedefs.h:95).
 *     Here every function returns DFXDSP_OK (0) or DFXDSP_ERR (-1) instead.
 *     Also note upstream NOT_OKAY expands to a function call in debug builds.
 *
 *  2. The sample pointer is typed `short int*` upstream, but the engine always
 *     operates on 32-bit float (`// Format will always be 32 bit floating
 *     point`, DfxDspPrivate.cpp:184). We expose `float*` and cast internally.
 *     Pass bits_per_sample = 32 to dfxdsp_set_signal_format().
 * ---------------------------------------------------------------------------
 */

#ifndef FXTRUMPET_DFXDSP_CAPI_H
#define FXTRUMPET_DFXDSP_CAPI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DFXDSP_OK   0
#define DFXDSP_ERR (-1)

typedef void *DfxHandle;

/* Mirrors DfxDsp::Effect. Keep in sync with the enum in DfxDsp.h:38. */
typedef enum {
    DFX_EFFECT_FIDELITY      = 0,
    DFX_EFFECT_AMBIENCE      = 1,
    DFX_EFFECT_SURROUND      = 2,
    DFX_EFFECT_DYNAMIC_BOOST = 3,
    DFX_EFFECT_BASS          = 4,
    DFX_EFFECT_COUNT         = 5
} DfxEffectId;

/* ── lifecycle ────────────────────────────────────────────────────────── */

/* Returns NULL on allocation failure. */
DfxHandle dfxdsp_create(void);
void      dfxdsp_destroy(DfxHandle h);

/* ── signal format & processing ───────────────────────────────────────── */

/* Call whenever the stream format changes. bits_per_sample must be 32 (float).
   The first call after construction is expected to report DFXDSP_ERR upstream
   behaviour; treat it as informational, not fatal. */
int dfxdsp_set_signal_format(DfxHandle h, int bits_per_sample, int channels,
                             int sample_rate, int valid_bits);

/* In-place processing. `inout` holds num_frames * channels interleaved float32
   samples. num_frames is FRAMES, not total samples — upstream assigns it from
   capturedFramesCount (sndDevicesDoCapture.cpp:385).
   Must be called from a single thread. Never allocates, but is not reentrant. */
void dfxdsp_process(DfxHandle h, float *inout, int num_frames);

/* Out-of-place variant. `out` may alias `in`, though dfxdsp_process is cheaper. */
void dfxdsp_process_separate(DfxHandle h, const float *in, float *out,
                             int num_frames);

/* ── presets (.fac). All path/name strings are UTF-16, NUL-terminated. ── */

int dfxdsp_load_preset(DfxHandle h, const uint16_t *utf16_path);
int dfxdsp_save_preset(DfxHandle h, const uint16_t *utf16_name,
                       const uint16_t *utf16_path);
int dfxdsp_export_preset(DfxHandle h, const uint16_t *utf16_src_path,
                         const uint16_t *utf16_name,
                         const uint16_t *utf16_export_path);

/* Reads only the preset's display name without applying it.
   Writes at most out_name_capacity uint16 units (always NUL-terminated).
   Returns the name length in units, or DFXDSP_ERR. */
int dfxdsp_preset_name(DfxHandle h, const uint16_t *utf16_path,
                       uint16_t *out_name, int out_name_capacity);

/* ── power / EQ enable ────────────────────────────────────────────────── */

/* Power state.
   Upstream's DfxDsp::isPowerOn() is INVERTED: it reads the BYPASS button and
   returns true when that value is non-zero — i.e. it reports "bypassed" under
   the name "power on" (DfxDspPrivate.cpp:216). This wrapper corrects that, so
   dfxdsp_set_power(h, 1) is followed by dfxdsp_is_power_on(h) == 1. */
void dfxdsp_set_power(DfxHandle h, int on);
int  dfxdsp_is_power_on(DfxHandle h);

void dfxdsp_set_eq_on(DfxHandle h, int on);

/* ── equalizer ────────────────────────────────────────────────────────── */

/* Valid band counts are 5, 10, 15, 20, 31. Default is 10. */
int  dfxdsp_num_bands(DfxHandle h);
void dfxdsp_set_num_bands(DfxHandle h, int num_bands);

double dfxdsp_band_freq(DfxHandle h, int band);
void   dfxdsp_set_band_freq(DfxHandle h, int band, double freq);
int    dfxdsp_band_freq_range(DfxHandle h, int band, double *out_min,
                              double *out_max);

/* Gain in dB, valid range -12.0 .. +12.0. */
double dfxdsp_band_gain(DfxHandle h, int band);
void   dfxdsp_set_band_gain(DfxHandle h, int band, double db);

/* ── sound effects ────────────────────────────────────────────────────── */

/* The getter and setter use DIFFERENT scales. This asymmetry is upstream's, not
   an oversight in this wrapper (DfxDspPrivate.cpp:231 and :254):

     dfxdsp_set_effect()   takes  0.0 .. 10.0   slider units; upstream stores value/10
     dfxdsp_effect()       returns 0.0 .. 1.0   the normalised internal value

   `.fac` presets store the same knob as MIDI 0..127, which the loader writes
   straight into that internal field. So a preset's Main value N reads back as
   N/127 ≈ N/127, and round-tripping it through the setter means N/12.7.

   Example: Music.fac has Main 0 = 50 -> dfxdsp_effect() == 0.39 ->
   dfxdsp_set_effect(DFX_EFFECT_FIDELITY, 3.94) restores it. */
double dfxdsp_effect(DfxHandle h, DfxEffectId effect);
void   dfxdsp_set_effect(DfxHandle h, DfxEffectId effect, double value);

/* ── global params ────────────────────────────────────────────────────── */

/* balance / master_gain: -20.0 .. +20.0 dB */
double dfxdsp_balance(DfxHandle h);
void   dfxdsp_set_balance(DfxHandle h, double db);
double dfxdsp_master_gain(DfxHandle h);
void   dfxdsp_set_master_gain(DfxHandle h, double db);

/* normalization / volume_leveling: 0.0 .. 4.0 dB */
double dfxdsp_normalization(DfxHandle h);
void   dfxdsp_set_normalization(DfxHandle h, double db);
double dfxdsp_volume_leveling(DfxHandle h);
void   dfxdsp_set_volume_leveling(DfxHandle h, double db);

/* filter_q: 1.0 .. 3.0 */
double dfxdsp_filter_q(DfxHandle h);
void   dfxdsp_set_filter_q(DfxHandle h, double q);

/* ── metering ─────────────────────────────────────────────────────────── */

/* Fills `count` magnitude values (upstream uses 10) for a spectrum display.
   Safe to call from the UI thread while the audio thread is processing. */
void dfxdsp_spectrum(DfxHandle h, float *out_bands, int count);

uint32_t dfxdsp_total_processed_ms(DfxHandle h);
void     dfxdsp_reset_processed_ms(DfxHandle h);

/* ── introspection ────────────────────────────────────────────────────── */

int dfxdsp_effect_count(void);

#ifdef __cplusplus
}
#endif

#endif /* FXTRUMPET_DFXDSP_CAPI_H */
