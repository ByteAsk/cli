// Region TMA test program: two regions with opposite bottlenecks, each run
// under the perf control fifo only when PERFLAB_REGION names it.
//   compute: a dependent multiply/xorshift chain  -> core-bound
//   memory:  dependent random loads, 64 MiB table -> memory-bound
//   g++ -O2 -std=c++20 -o regions regions.cpp
//   ./regions [--perf-ctl CTL ACK]      (PERFLAB_REGION=compute|memory)
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <unistd.h>
#include <vector>

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
}  // namespace

int main(int argc, char** argv) {
    PerfCtl pc;
    for (int i = 1; i < argc; ++i) {
        if (std::string(argv[i]) == "--perf-ctl" && i + 2 < argc) {
            pc.ctl = open(argv[++i], O_WRONLY);
            pc.ack = open(argv[++i], O_RDONLY);
        }
    }
    const char* r = std::getenv("PERFLAB_REGION");
    const std::string region = r ? r : "compute";
    // 64 MiB of 32-bit next-indices forming one random cycle (Sattolo).
    const size_t n = size_t(1) << 24;
    std::vector<uint32_t> next(n);
    for (size_t i = 0; i < n; ++i) next[i] = uint32_t(i);
    uint64_t s = 88172645463325252ull;
    for (size_t i = n - 1; i > 0; --i) {
        s ^= s << 13; s ^= s >> 7; s ^= s << 17;
        size_t j = s % i;
        std::swap(next[i], next[j]);
    }
    uint64_t x = 1, sink = 0;
    if (region == "compute") pc.cmd("enable\n");
    for (long i = 0; i < 200'000'000; ++i) {
        x = x * 6364136223846793005ull + 1442695040888963407ull;
        x ^= x >> 29;
    }
    if (region == "compute") pc.cmd("disable\n");
    sink += x;
    if (region == "memory") pc.cmd("enable\n");
    uint32_t p = 0;
    for (long i = 0; i < 20'000'000; ++i) p = next[p];
    if (region == "memory") pc.cmd("disable\n");
    sink += p;
    std::printf("sink %llu\nPERFLAB {\"regions_ok\": 1}\n", (unsigned long long)(sink & 0xff));
    return 0;
}
