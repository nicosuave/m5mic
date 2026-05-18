#ifndef M5MIC_IOS_CORE_H
#define M5MIC_IOS_CORE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    M5MIC_OK = 0,
    M5MIC_INCOMPLETE = 1,
    M5MIC_STREAM_STARTED = 2,
    M5MIC_STREAM_AUDIO = 3,
    M5MIC_STREAM_ENDED = 4,

    M5MIC_ERROR_NULL = -1,
    M5MIC_ERROR_PROTOCOL = -2,
    M5MIC_ERROR_UNSUPPORTED_FORMAT = -3,
    M5MIC_ERROR_OUTPUT_TOO_SMALL = -4,
    M5MIC_ERROR_FRAGMENT_TOO_LARGE = -5
};

typedef struct M5MicDecoder M5MicDecoder;
typedef struct M5MicBleReassembler M5MicBleReassembler;

uint16_t m5mic_discovery_port(void);
uint16_t m5mic_control_port(void);
uint16_t m5mic_ws_port(void);
const char *m5mic_ws_path(void);
const char *m5mic_bonjour_type(void);
const char *m5mic_bonjour_service_type(void);
const char *m5mic_ble_service_uuid(void);
const char *m5mic_ble_audio_characteristic_uuid(void);
const char *m5mic_ble_control_characteristic_uuid(void);
const char *m5mic_ble_status_characteristic_uuid(void);
const uint8_t *m5mic_discovery_request(size_t *len);
const char *m5mic_discovery_response_prefix(void);
const uint8_t *m5mic_control_mode_wifi(size_t *len);
const uint8_t *m5mic_control_mode_ble(size_t *len);
const uint8_t *m5mic_control_mode_usb(size_t *len);
const uint8_t *m5mic_control_record_start(size_t *len);
const uint8_t *m5mic_control_record_stop(size_t *len);
size_t m5mic_default_frame_capacity(void);
size_t m5mic_default_output_sample_capacity(void);

M5MicDecoder *m5mic_decoder_new(void);
void m5mic_decoder_free(M5MicDecoder *decoder);
void m5mic_decoder_reset(M5MicDecoder *decoder);

int32_t m5mic_decode_frame(
    M5MicDecoder *decoder,
    const uint8_t *frame,
    size_t frame_len,
    float *out_samples,
    size_t out_sample_capacity,
    size_t *out_sample_len,
    uint32_t *stream_id,
    uint8_t *level,
    uint32_t *sample_rate,
    uint8_t *channels,
    uint16_t *flags
);

M5MicBleReassembler *m5mic_ble_reassembler_new(void);
void m5mic_ble_reassembler_free(M5MicBleReassembler *reassembler);
void m5mic_ble_reassembler_reset(M5MicBleReassembler *reassembler);

int32_t m5mic_ble_reassembler_push(
    M5MicBleReassembler *reassembler,
    const uint8_t *fragment,
    size_t fragment_len,
    uint8_t *out_frame,
    size_t out_frame_capacity,
    size_t *out_frame_len
);

#ifdef __cplusplus
}
#endif

#endif
