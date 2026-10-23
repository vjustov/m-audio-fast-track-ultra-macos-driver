// ftu_stream: Phase 6 streaming prototype for the M-Audio Fast Track Ultra.
//
// Plays a sine tone on one DAW channel (isochronous OUT 0x01) and records all
// 8 inputs (isochronous IN 0x81) to a WAV file. Playback packet sizes follow
// the capture packet sizes (implicit feedback, as Linux does).
//
// Build: make -C proto
// Usage: ftu_stream [-r rate] [-t seconds] [-c channel 1-8] [-f freq] [-a dBFS] [-o out.wav] [-q]
//        -q skips the once-per-second meter readout.
//        -s selects the S/PDIF clock (capture alt setting 2).

#include <libusb.h>
#include <math.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define VID 0x0763
#define PID 0x2080
#define IF_PLAY 1
#define IF_CAP 2
#define EP_PLAY 0x01
#define EP_CAP 0x81
#define CHANNELS 8
#define BYTES_PER_SAMPLE 3
#define FRAME_BYTES (CHANNELS * BYTES_PER_SAMPLE)
#define MAX_PACKET 312
#define PACKETS_PER_XFER 32
#define NUM_XFERS 12
#define FIFO_SIZE 4096

static libusb_device_handle *dev;
static volatile int running = 1;

static int rate = 48000;
static double seconds = 5.0;
static int tone_channel = 1;
static double tone_freq = 440.0;
static double tone_dbfs = -30.0;
static const char *wav_path = "capture.wav";
static int show_meters = 1;
static int capture_alt = 1;  // 1 = internal clock, 2 = S/PDIF clock

static double phase;
static double amplitude;
static FILE *wav;
static uint64_t wav_bytes;

// Frame counts from completed capture packets, consumed by playback packets.
static int fifo[FIFO_SIZE];
static unsigned fifo_head, fifo_tail;

// Fallback when the fifo is empty: nominal frames per microframe.
static double nominal_acc;

static struct {
    uint64_t in_packets, in_frames, in_errors, in_empty;
    uint64_t out_packets, out_frames, out_errors, out_fallback;
    uint64_t size_hist[MAX_PACKET / FRAME_BYTES + 2];
    int32_t peak[CHANNELS];
} st;

static void on_signal(int sig) {
    (void)sig;
    running = 0;
}

static int next_frame_count(void) {
    if (fifo_tail != fifo_head) {
        int n = fifo[fifo_tail % FIFO_SIZE];
        fifo_tail++;
        return n;
    }
    st.out_fallback++;
    nominal_acc += rate / 8000.0;
    int n = (int)nominal_acc;
    nominal_acc -= n;
    return n;
}

static void fill_tone(uint8_t *p, int frames) {
    double step = 2.0 * M_PI * tone_freq / rate;
    for (int f = 0; f < frames; f++) {
        int32_t v = (int32_t)(amplitude * sin(phase) * 8388607.0);
        phase += step;
        if (phase > 2.0 * M_PI) phase -= 2.0 * M_PI;
        for (int c = 0; c < CHANNELS; c++) {
            int32_t s = (c == tone_channel - 1) ? v : 0;
            p[0] = s & 0xFF;
            p[1] = (s >> 8) & 0xFF;
            p[2] = (s >> 16) & 0xFF;
            p += BYTES_PER_SAMPLE;
        }
    }
}

static void prepare_out(struct libusb_transfer *t) {
    int offset = 0;
    for (int i = 0; i < PACKETS_PER_XFER; i++) {
        int frames = next_frame_count();
        int len = frames * FRAME_BYTES;
        fill_tone(t->buffer + offset, frames);
        t->iso_packet_desc[i].length = len;
        offset += len;
        st.out_frames += frames;
    }
    t->length = offset;
}

static void LIBUSB_CALL out_cb(struct libusb_transfer *t) {
    if (t->status != LIBUSB_TRANSFER_COMPLETED) {
        st.out_errors++;
        fprintf(stderr, "playback transfer status %d\n", t->status);
    }
    for (int i = 0; i < t->num_iso_packets; i++)
        if (t->iso_packet_desc[i].status != LIBUSB_TRANSFER_COMPLETED) st.out_errors++;
    st.out_packets += t->num_iso_packets;
    if (!running) return;
    prepare_out(t);
    int r = libusb_submit_transfer(t);
    if (r < 0) {
        fprintf(stderr, "playback resubmit failed: %s\n", libusb_error_name(r));
        running = 0;
    }
}

static void LIBUSB_CALL in_cb(struct libusb_transfer *t) {
    if (t->status != LIBUSB_TRANSFER_COMPLETED) {
        st.in_errors++;
        fprintf(stderr, "capture transfer status %d\n", t->status);
    }
    for (int i = 0; i < t->num_iso_packets; i++) {
        struct libusb_iso_packet_descriptor *d = &t->iso_packet_desc[i];
        st.in_packets++;
        if (d->status != LIBUSB_TRANSFER_COMPLETED) {
            st.in_errors++;
            continue;
        }
        int frames = d->actual_length / FRAME_BYTES;
        if (frames == 0) st.in_empty++;
        if (frames <= MAX_PACKET / FRAME_BYTES) st.size_hist[frames]++;
        st.in_frames += frames;
        if (fifo_head - fifo_tail < FIFO_SIZE) fifo[(fifo_head++) % FIFO_SIZE] = frames;

        uint8_t *p = libusb_get_iso_packet_buffer_simple(t, i);
        for (int f = 0; f < frames; f++)
            for (int c = 0; c < CHANNELS; c++) {
                uint8_t *s = p + f * FRAME_BYTES + c * BYTES_PER_SAMPLE;
                int32_t v = (int32_t)((uint32_t)s[0] << 8 | (uint32_t)s[1] << 16 | (uint32_t)s[2] << 24) >> 8;
                if (abs(v) > st.peak[c]) st.peak[c] = abs(v);
            }
        if (wav && frames) {
            fwrite(p, FRAME_BYTES, frames, wav);
            wav_bytes += (uint64_t)frames * FRAME_BYTES;
        }
    }
    if (!running) return;
    int r = libusb_submit_transfer(t);
    if (r < 0) {
        fprintf(stderr, "capture resubmit failed: %s\n", libusb_error_name(r));
        running = 0;
    }
}

static void put_le(FILE *f, uint32_t v, int n) {
    for (int i = 0; i < n; i++) fputc((v >> (8 * i)) & 0xFF, f);
}

static void wav_header(FILE *f, uint32_t data_bytes) {
    fseek(f, 0, SEEK_SET);
    fwrite("RIFF", 1, 4, f);
    put_le(f, 36 + data_bytes, 4);
    fwrite("WAVEfmt ", 1, 8, f);
    put_le(f, 16, 4);
    put_le(f, 1, 2);
    put_le(f, CHANNELS, 2);
    put_le(f, rate, 4);
    put_le(f, rate * FRAME_BYTES, 4);
    put_le(f, FRAME_BYTES, 2);
    put_le(f, 24, 2);
    fwrite("data", 1, 4, f);
    put_le(f, data_bytes, 4);
}

static int set_rate(int hz) {
    uint8_t d[3] = {hz & 0xFF, (hz >> 8) & 0xFF, (hz >> 16) & 0xFF};
    return libusb_control_transfer(dev, 0x22, 0x01, 0x0100, EP_CAP, d, 3, 1000);
}

static int get_rate(void) {
    uint8_t d[3] = {0};
    if (libusb_control_transfer(dev, 0xA2, 0x81, 0x0100, EP_CAP, d, 3, 1000) != 3) return -1;
    return d[0] | d[1] << 8 | d[2] << 16;
}

// GET_MEM returns the snapshot taken at the previous read, so read twice.
static int read_meters(int16_t w[27]) {
    uint8_t b[54];
    for (int i = 0; i < 2; i++)
        if (libusb_control_transfer(dev, 0xA1, 0x85, 0x0100, 0x0504, b, 54, 500) != 54) return -1;
    for (int i = 0; i < 27; i++) w[i] = (int16_t)(b[2 * i] | b[2 * i + 1] << 8);
    return 0;
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void usage(const char *argv0) {
    fprintf(stderr, "usage: %s [-r rate] [-t seconds] [-c channel 1-8] [-f freq] [-a dBFS] [-o out.wav] [-q] [-s]\n", argv0);
    exit(2);
}

int main(int argc, char **argv) {
    int opt;
    while ((opt = getopt(argc, argv, "r:t:c:f:a:o:qs")) != -1) {
        switch (opt) {
        case 'r': rate = atoi(optarg); break;
        case 't': seconds = atof(optarg); break;
        case 'c': tone_channel = atoi(optarg); break;
        case 'f': tone_freq = atof(optarg); break;
        case 'a': tone_dbfs = atof(optarg); break;
        case 'o': wav_path = optarg; break;
        case 'q': show_meters = 0; break;
        case 's': capture_alt = 2; break;
        default: usage(argv[0]);
        }
    }
    if (tone_channel < 0 || tone_channel > CHANNELS) usage(argv[0]);
    amplitude = pow(10.0, tone_dbfs / 20.0);
    signal(SIGINT, on_signal);

    if (libusb_init(NULL) < 0) return 1;
    dev = libusb_open_device_with_vid_pid(NULL, VID, PID);
    if (!dev) {
        fprintf(stderr, "device %04x:%04x not found\n", VID, PID);
        return 1;
    }
    int r;
    if ((r = libusb_claim_interface(dev, IF_PLAY)) < 0 || (r = libusb_claim_interface(dev, IF_CAP)) < 0) {
        fprintf(stderr, "claim failed: %s\n", libusb_error_name(r));
        return 1;
    }
    if ((r = libusb_set_interface_alt_setting(dev, IF_PLAY, 1)) < 0 ||
        (r = libusb_set_interface_alt_setting(dev, IF_CAP, capture_alt)) < 0) {
        fprintf(stderr, "alt setting failed: %s\n", libusb_error_name(r));
        return 1;
    }
    if (set_rate(rate) < 0) fprintf(stderr, "warning: set rate failed\n");
    printf("rate requested %d, device reports %d\n", rate, get_rate());
    printf("tone: channel %d, %.1f Hz, %.1f dBFS\n", tone_channel, tone_freq, tone_dbfs);

    wav = fopen(wav_path, "wb");
    if (wav) wav_header(wav, 0);

    struct libusb_transfer *in[NUM_XFERS], *out[NUM_XFERS];
    for (int i = 0; i < NUM_XFERS; i++) {
        in[i] = libusb_alloc_transfer(PACKETS_PER_XFER);
        uint8_t *b = malloc(MAX_PACKET * PACKETS_PER_XFER);
        libusb_fill_iso_transfer(in[i], dev, EP_CAP, b, MAX_PACKET * PACKETS_PER_XFER, PACKETS_PER_XFER, in_cb, NULL, 1000);
        libusb_set_iso_packet_lengths(in[i], MAX_PACKET);
        libusb_submit_transfer(in[i]);
    }
    for (int i = 0; i < NUM_XFERS; i++) {
        out[i] = libusb_alloc_transfer(PACKETS_PER_XFER);
        uint8_t *b = malloc(MAX_PACKET * PACKETS_PER_XFER);
        libusb_fill_iso_transfer(out[i], dev, EP_PLAY, b, 0, PACKETS_PER_XFER, out_cb, NULL, 1000);
        prepare_out(out[i]);
        libusb_submit_transfer(out[i]);
    }

    double start = now(), last = start;
    while (running && now() - start < seconds) {
        struct timeval tv = {0, 100000};
        libusb_handle_events_timeout(NULL, &tv);
        if (show_meters && now() - last >= 1.0) {
            last = now();
            int16_t w[27];
            if (read_meters(w) == 0)
                printf("t=%4.1fs in1=%5d daw1-8=[%d %d %d %d %d %d %d %d] out1-4=[%d %d %d %d] status=0x%04x\n",
                       last - start, abs(w[0]), abs(w[8]), abs(w[9]), abs(w[10]), abs(w[11]), abs(w[12]),
                       abs(w[13]), abs(w[14]), abs(w[15]), abs(w[16]), abs(w[17]), abs(w[18]), abs(w[19]),
                       (uint16_t)w[26]);
            fflush(stdout);
        }
    }
    if (running == 0) fprintf(stderr, "stopped early at %.2fs\n", now() - start);
    running = 0;
    double elapsed = now() - start;

    // Drain: let in-flight transfers complete without resubmitting.
    double drain = now();
    while (now() - drain < 0.3) {
        struct timeval tv = {0, 50000};
        libusb_handle_events_timeout(NULL, &tv);
    }

    if (wav) {
        wav_header(wav, (uint32_t)wav_bytes);
        fclose(wav);
    }
    libusb_set_interface_alt_setting(dev, IF_PLAY, 0);
    libusb_set_interface_alt_setting(dev, IF_CAP, 0);
    libusb_release_interface(dev, IF_PLAY);
    libusb_release_interface(dev, IF_CAP);
    libusb_close(dev);
    libusb_exit(NULL);

    printf("\nelapsed %.2fs\n", elapsed);
    printf("capture : %llu packets, %llu frames (%.1f Hz), %llu errors, %llu empty\n",
           st.in_packets, st.in_frames, st.in_frames / elapsed, st.in_errors, st.in_empty);
    printf("playback: %llu packets, %llu frames (%.1f Hz), %llu errors, %llu fallback packets\n",
           st.out_packets, st.out_frames, st.out_frames / elapsed, st.out_errors, st.out_fallback);
    printf("capture packet sizes (frames:count):");
    for (int i = 0; i <= MAX_PACKET / FRAME_BYTES; i++)
        if (st.size_hist[i]) printf(" %d:%llu", i, st.size_hist[i]);
    printf("\ncapture peaks dBFS:");
    for (int c = 0; c < CHANNELS; c++)
        printf(" %.1f", st.peak[c] ? 20 * log10(st.peak[c] / 8388607.0) : -144.0);
    printf("\nwav: %s (%llu bytes of audio)\n", wav_path, wav_bytes);
    return 0;
}
