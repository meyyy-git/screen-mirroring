// Jembatan tipis ke decoder AAC milik FFmpeg (libavcodec). WebView2 tidak bisa men-decode AAC-ELD,
// format suara yang dipakai iPhone saat screen mirroring, jadi decode dilakukan di sini.
#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/mem.h>
#include <string.h>

typedef struct {
    AVCodecContext *ctx;
    AVPacket *pkt;
    AVFrame *frame;
} SmAac;

void sm_aac_close(SmAac *d) {
    if (!d) return;
    av_frame_free(&d->frame);
    av_packet_free(&d->pkt);
    avcodec_free_context(&d->ctx);
    av_free(d);
}

// asc = AudioSpecificConfig (menentukan profil: ELD, LC, ...). NULL kalau gagal.
SmAac *sm_aac_open(const uint8_t *asc, int asc_len, int sample_rate, int channels) {
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_AAC);
    SmAac *d = codec ? av_mallocz(sizeof(*d)) : NULL;
    if (!d) return NULL;
    d->ctx = avcodec_alloc_context3(codec);
    d->pkt = av_packet_alloc();
    d->frame = av_frame_alloc();
    if (!d->ctx || !d->pkt || !d->frame) goto fail;
    d->ctx->extradata = av_mallocz(asc_len + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!d->ctx->extradata) goto fail;
    memcpy(d->ctx->extradata, asc, asc_len);
    d->ctx->extradata_size = asc_len;
    d->ctx->sample_rate = sample_rate;
    av_channel_layout_default(&d->ctx->ch_layout, channels);
    if (avcodec_open2(d->ctx, codec, NULL) < 0) goto fail;
    return d;
fail:
    sm_aac_close(d);
    return NULL;
}

// Men-decode satu frame AAC. `out` menerima float planar (semua sampel kanal 0, lalu kanal 1, ...).
// Mengembalikan jumlah sampel per kanal, 0 kalau decoder belum mengeluarkan apa-apa, negatif kalau gagal.
int sm_aac_decode(SmAac *d, const uint8_t *data, int len, float *out, int out_cap,
                  int *channels, int *sample_rate) {
    if (av_new_packet(d->pkt, len) < 0) return -1;
    memcpy(d->pkt->data, data, len);
    int r = avcodec_send_packet(d->ctx, d->pkt);
    av_packet_unref(d->pkt);
    if (r < 0) return -1;
    r = avcodec_receive_frame(d->ctx, d->frame);
    if (r == AVERROR(EAGAIN)) return 0;
    if (r < 0) return -1;

    int n = d->frame->nb_samples;
    int ch = d->frame->ch_layout.nb_channels;
    if (d->frame->format != AV_SAMPLE_FMT_FLTP || n * ch > out_cap) {
        av_frame_unref(d->frame);
        return -1;
    }
    for (int c = 0; c < ch; c++)
        memcpy(out + (size_t)c * n, d->frame->extended_data[c], (size_t)n * sizeof(float));
    *channels = ch;
    *sample_rate = d->frame->sample_rate;
    av_frame_unref(d->frame);
    return n;
}
