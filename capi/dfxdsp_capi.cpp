/*
 * FxTrumpet — implementation of the C ABI declared in dfxdsp_capi.h.
 *
 * Thin forwarding layer: every function unwraps the opaque handle, bridges the
 * UTF-16 strings to std::wstring, and normalises upstream's inverted return
 * codes (upstream `OKAY` is 0, see codedefs.h:95).
 *
 * Derived from FxSound, Copyright (C) 2025 FxSound LLC.
 * Licensed under the GNU Affero General Public License v3.0, same as upstream.
 */

#include "dfxdsp_capi.h"

#include "DfxDsp.h"

#include <string>

namespace {

/* Upstream success code. Deliberately not included from codedefs.h so this
   translation unit does not pull in the debug-build NOT_OKAY macro, which
   expands to a function call and would break the `return` below. */
const int kUpstreamOkay = 0;

inline int normalise(int upstream_result) {
    return upstream_result == kUpstreamOkay ? DFXDSP_OK : DFXDSP_ERR;
}

/* UTF-16 from Rust -> std::wstring. wchar_t is 16-bit on MSVC. */
inline std::wstring to_wide(const uint16_t *utf16) {
    if (utf16 == nullptr) {
        return std::wstring();
    }
    return std::wstring(reinterpret_cast<const wchar_t *>(utf16));
}

inline DfxDsp *cast(DfxHandle h) {
    return static_cast<DfxDsp *>(h);
}

inline int clamp_int(int value, int lo, int hi) {
    if (value < lo) return lo;
    if (value > hi) return hi;
    return value;
}

}  // namespace

/* Upstream DfxDsp::processAudio takes the frame count; the buffer is always
   float32 regardless of the `short int *` spelling. */
static const int kCheckForDuplicateBuffers = 0; /* IS_FALSE */

extern "C" {

/* ── lifecycle ────────────────────────────────────────────────────────── */

DfxHandle dfxdsp_create(void) {
    return static_cast<DfxHandle>(new (std::nothrow) DfxDsp());
}

void dfxdsp_destroy(DfxHandle h) {
    delete cast(h);
}

/* ── signal format & processing ───────────────────────────────────────── */

int dfxdsp_set_signal_format(DfxHandle h, int bits_per_sample, int channels,
                             int sample_rate, int valid_bits) {
    if (h == nullptr) return DFXDSP_ERR;
    return normalise(cast(h)->setSignalFormat(bits_per_sample, channels,
                                              sample_rate, valid_bits));
}

void dfxdsp_process(DfxHandle h, float *inout, int num_frames) {
    if (h == nullptr || inout == nullptr || num_frames <= 0) return;
    short int *buffer = reinterpret_cast<short int *>(inout);
    cast(h)->processAudio(buffer, buffer, num_frames, kCheckForDuplicateBuffers);
}

void dfxdsp_process_separate(DfxHandle h, const float *in, float *out,
                             int num_frames) {
    if (h == nullptr || in == nullptr || out == nullptr || num_frames <= 0) return;
    cast(h)->processAudio(reinterpret_cast<short int *>(const_cast<float *>(in)),
                          reinterpret_cast<short int *>(out), num_frames,
                          kCheckForDuplicateBuffers);
}

/* ── presets ──────────────────────────────────────────────────────────── */

int dfxdsp_load_preset(DfxHandle h, const uint16_t *utf16_path) {
    if (h == nullptr || utf16_path == nullptr) return DFXDSP_ERR;
    return normalise(cast(h)->loadPreset(to_wide(utf16_path)));
}

int dfxdsp_save_preset(DfxHandle h, const uint16_t *utf16_name,
                       const uint16_t *utf16_path) {
    if (h == nullptr || utf16_name == nullptr || utf16_path == nullptr) {
        return DFXDSP_ERR;
    }
    return normalise(
        cast(h)->savePreset(to_wide(utf16_name), to_wide(utf16_path)));
}

int dfxdsp_export_preset(DfxHandle h, const uint16_t *utf16_src_path,
                         const uint16_t *utf16_name,
                         const uint16_t *utf16_export_path) {
    if (h == nullptr || utf16_src_path == nullptr || utf16_name == nullptr ||
        utf16_export_path == nullptr) {
        return DFXDSP_ERR;
    }
    return normalise(cast(h)->exportPreset(
        to_wide(utf16_src_path), to_wide(utf16_name), to_wide(utf16_export_path)));
}

int dfxdsp_preset_name(DfxHandle h, const uint16_t *utf16_path,
                       uint16_t *out_name, int out_name_capacity) {
    if (h == nullptr || utf16_path == nullptr || out_name == nullptr ||
        out_name_capacity <= 0) {
        return DFXDSP_ERR;
    }

    DfxPreset info = cast(h)->getPresetInfo(to_wide(utf16_path));
    const std::wstring &name = info.name;

    const int capacity = out_name_capacity;
    const int source_len = static_cast<int>(name.size());
    const int copy_len = source_len < capacity - 1 ? source_len : capacity - 1;

    for (int i = 0; i < copy_len; ++i) {
        out_name[i] = static_cast<uint16_t>(name[static_cast<size_t>(i)]);
    }
    out_name[copy_len] = 0;

    return copy_len;
}

/* ── power / EQ enable ────────────────────────────────────────────────── */

void dfxdsp_set_power(DfxHandle h, int on) {
    if (h == nullptr) return;
    cast(h)->powerOn(on != 0);
}

int dfxdsp_is_power_on(DfxHandle h) {
    if (h == nullptr) return 0;
    // Upstream DfxDsp::isPowerOn() reports the BYPASS button: it returns true
    // when bypass is non-zero, i.e. true when processing is OFF. powerOn(true)
    // sets bypass to 0, so upstream's getter and setter disagree with each other.
    // Invert here so this API is self-consistent.
    return cast(h)->isPowerOn() ? 0 : 1;
}

void dfxdsp_set_eq_on(DfxHandle h, int on) {
    if (h == nullptr) return;
    cast(h)->eqOn(on != 0);
}

/* ── equalizer ────────────────────────────────────────────────────────── */

int dfxdsp_num_bands(DfxHandle h) {
    if (h == nullptr) return 0;
    return cast(h)->getNumEqBands();
}

void dfxdsp_set_num_bands(DfxHandle h, int num_bands) {
    if (h == nullptr) return;
    cast(h)->setNumBands(num_bands);
}

double dfxdsp_band_freq(DfxHandle h, int band) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getEqBandFrequency(band));
}

void dfxdsp_set_band_freq(DfxHandle h, int band, double freq) {
    if (h == nullptr) return;
    cast(h)->setEqBandFrequency(band, static_cast<float>(freq));
}

int dfxdsp_band_freq_range(DfxHandle h, int band, double *out_min,
                           double *out_max) {
    if (h == nullptr || out_min == nullptr || out_max == nullptr) {
        return DFXDSP_ERR;
    }
    float min_freq = 0.0f;
    float max_freq = 0.0f;
    cast(h)->getEqBandFrequencyRange(band, &min_freq, &max_freq);
    *out_min = static_cast<double>(min_freq);
    *out_max = static_cast<double>(max_freq);
    return DFXDSP_OK;
}

double dfxdsp_band_gain(DfxHandle h, int band) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getEqBandBoostCut(band));
}

void dfxdsp_set_band_gain(DfxHandle h, int band, double db) {
    if (h == nullptr) return;
    cast(h)->setEqBandBoostCut(band, static_cast<float>(db));
}

/* ── sound effects ────────────────────────────────────────────────────── */

double dfxdsp_effect(DfxHandle h, DfxEffectId effect) {
    if (h == nullptr || effect < 0 || effect >= DFX_EFFECT_COUNT) return 0.0;
    return static_cast<double>(
        cast(h)->getEffectValue(static_cast<DfxDsp::Effect>(effect)));
}

void dfxdsp_set_effect(DfxHandle h, DfxEffectId effect, double value) {
    if (h == nullptr || effect < 0 || effect >= DFX_EFFECT_COUNT) return;
    cast(h)->setEffectValue(static_cast<DfxDsp::Effect>(effect),
                            static_cast<float>(value));
}

/* ── global params ────────────────────────────────────────────────────── */

double dfxdsp_balance(DfxHandle h) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getBalance());
}

void dfxdsp_set_balance(DfxHandle h, double db) {
    if (h == nullptr) return;
    cast(h)->setBalance(static_cast<float>(db));
}

double dfxdsp_master_gain(DfxHandle h) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getMasterGain());
}

void dfxdsp_set_master_gain(DfxHandle h, double db) {
    if (h == nullptr) return;
    cast(h)->setMasterGain(static_cast<float>(db));
}

double dfxdsp_normalization(DfxHandle h) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getNormalization());
}

void dfxdsp_set_normalization(DfxHandle h, double db) {
    if (h == nullptr) return;
    cast(h)->setNormalization(static_cast<float>(db));
}

double dfxdsp_volume_leveling(DfxHandle h) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getVolumeLeveling());
}

void dfxdsp_set_volume_leveling(DfxHandle h, double db) {
    if (h == nullptr) return;
    cast(h)->setVolumeLeveling(static_cast<float>(db));
}

double dfxdsp_filter_q(DfxHandle h) {
    if (h == nullptr) return 0.0;
    return static_cast<double>(cast(h)->getFilterQ());
}

void dfxdsp_set_filter_q(DfxHandle h, double q) {
    if (h == nullptr) return;
    cast(h)->setFilterQ(static_cast<float>(q));
}

/* ── metering ─────────────────────────────────────────────────────────── */

void dfxdsp_spectrum(DfxHandle h, float *out_bands, int count) {
    if (h == nullptr || out_bands == nullptr || count <= 0) return;
    cast(h)->getSpectrumBandValues(out_bands, count);
}

uint32_t dfxdsp_total_processed_ms(DfxHandle h) {
    if (h == nullptr) return 0;
    return static_cast<uint32_t>(cast(h)->getTotalAudioProcessedTime());
}

void dfxdsp_reset_processed_ms(DfxHandle h) {
    if (h == nullptr) return;
    cast(h)->resetTotalAudioProcessedTime();
}

/* ── introspection ────────────────────────────────────────────────────── */

int dfxdsp_effect_count(void) {
    return static_cast<int>(DfxDsp::NumEffects);
}

}  /* extern "C" */
