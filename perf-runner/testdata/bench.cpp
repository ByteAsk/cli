// perf-runner's own test benchmark: speaks the runner's protocol exactly as a
// customer benchmark would (per-iteration latency, PERFLAB line, perf control
// fifos). `--slowdown-pct P` plants a proportional slowdown for the
// "injected slowdown is detected" check.
//
//   g++ -O2 -std=c++20 -o bench bench.cpp
//   ./bench [--iters N] [--work K] [--slowdown-pct P] [--perf-ctl CTL ACK]
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <unistd.h>
#include <vector>
#include <x86intrin.h>

namespace {

struct PerfCtl {
    int ctl = -1, ack = -1;
    void cmd(const char* s) const {
        if (ctl < 0) return;
        if (write(ctl, s, std::strlen(s)) < 0) std::perror("perf-ctl write");
        char buf[16];
        if (read(ack, buf, sizeof buf) < 0) std::perror("perf-ctl ack");
    }
};

double tsc_ghz() {
    using clk = std::chrono::steady_clock;
    unsigned aux;
    auto t0 = clk::now();
    uint64_t c0 = __rdtscp(&aux);
    while (clk::now() - t0 < std::chrono::milliseconds(50)) {
    }
    uint64_t c1 = __rdtscp(&aux);
    auto t1 = clk::now();
    return double(c1 - c0) / std::chrono::duration<double, std::nano>(t1 - t0).count();
}

}  // namespace

int main(int argc, char** argv) {
    long iters = 2'000'000, work = 64;
    double slowdown = 0.0;
    PerfCtl pc;
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if (a == "--iters" && i + 1 < argc) iters = std::atol(argv[++i]);
        else if (a == "--work" && i + 1 < argc) work = std::atol(argv[++i]);
        else if (a == "--slowdown-pct" && i + 1 < argc) slowdown = std::atof(argv[++i]);
        else if (a == "--perf-ctl" && i + 2 < argc) {
            pc.ctl = open(argv[++i], O_WRONLY);
            pc.ack = open(argv[++i], O_RDONLY);
            if (pc.ctl < 0 || pc.ack < 0) { std::perror("perf-ctl open"); return 2; }
        } else { std::fprintf(stderr, "unknown argument %s\n", a.c_str()); return 2; }
    }
    // Work per message: K table lookups + dependent hashing; the planted
    // slowdown adds proportionally more of the same work.
    const long k = work + long(work * slowdown / 100.0 + 0.5);
    std::vector<uint32_t> table(1 << 20);
    for (size_t i = 0; i < table.size(); ++i) table[i] = uint32_t(i * 2654435761u);
    const double ghz = tsc_ghz();
    std::vector<uint32_t> lat_cycles(iters);
    uint64_t x = 0x9e3779b97f4a7c15ull, sink = 0;
    unsigned aux;
    auto one = [&](long n) {
        for (long j = 0; j < n; ++j) {
            x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
            sink += table[(x * 0x2545F4914F6CDD1Dull) >> 44];
        }
    };
    for (long i = 0; i < iters / 10; ++i) one(k);  // warmup, not recorded
    pc.cmd("enable\n");
    auto t0 = std::chrono::steady_clock::now();
    for (long i = 0; i < iters; ++i) {
        uint64_t c0 = __rdtscp(&aux);
        one(k);
        uint64_t c1 = __rdtscp(&aux);
        lat_cycles[i] = uint32_t(c1 - c0);
    }
    auto t1 = std::chrono::steady_clock::now();
    pc.cmd("disable\n");
    const double secs = std::chrono::duration<double>(t1 - t0).count();
    auto q = [&](double p) {
        size_t idx = std::min(lat_cycles.size() - 1, size_t(p * double(lat_cycles.size() - 1)));
        std::nth_element(lat_cycles.begin(), lat_cycles.begin() + idx, lat_cycles.end());
        return double(lat_cycles[idx]) / ghz;
    };
    const double p50 = q(0.50), p99 = q(0.99), p999 = q(0.999);
    // PERFLAB_SAMPLES protocol: raw per-iteration latencies (u64 ns, little
    // endian) so the runner computes exact quantiles itself.
    if (const char* path = std::getenv("PERFLAB_SAMPLES")) {
        if (FILE* f = std::fopen(path, "wb")) {
            std::vector<uint64_t> ns(lat_cycles.size());
            for (size_t i = 0; i < ns.size(); ++i) ns[i] = uint64_t(double(lat_cycles[i]) / ghz + 0.5);
            std::fwrite(ns.data(), sizeof(uint64_t), ns.size(), f);
            std::fclose(f);
        }
    }
    std::printf("checksum %llu\n", (unsigned long long)(sink & 0xffff));
    std::printf("PERFLAB {\"p50_ns\": %.3f, \"p99_ns\": %.3f, \"p999_ns\": %.3f, \"throughput\": %.1f, "
                "\"throughput_unit\": \"msg/s\", \"allocs_per_msg\": 0}\n",
                p50, p99, p999, double(iters) / secs);
    return 0;
}
