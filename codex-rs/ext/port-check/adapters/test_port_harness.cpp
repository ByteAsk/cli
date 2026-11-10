// Tests for port_harness.hpp. Built and run by the Rust test suite (check_tests.rs):
//   g++ -std=c++17 -Wall -Wextra -Werror -pedantic test_port_harness.cpp && ./a.out
#include "port_harness.hpp"

#include <cmath>
#include <cstdio>
#include <limits>
#include <sstream>

static int failures = 0;
#define CHECK(cond)                                                     \
  do {                                                                  \
    if (!(cond)) {                                                      \
      std::fprintf(stderr, "FAILED line %d: %s\n", __LINE__, #cond);    \
      ++failures;                                                       \
    }                                                                   \
  } while (0)

template <class F>
static bool throws(F f) {
  try {
    f();
  } catch (const std::exception&) {
    return true;
  }
  return false;
}

int main() {
  using namespace byteask_port;
  // Events parse with levels in order and typed accessors.
  const Event ev = Event::parse("md step=4 ts=99 rec=2 sym=A kind=exec id=7 side=S px=-15 qty=3 bp1=10 bq1=1 ap1=12 aq1=5 ap2=13 aq2=6");
  CHECK(ev.type == "md" && ev.step == 4 && ev.ts == 99);
  CHECK(ev.sym() == "A" && ev.kind() == "exec" && ev.side() == 'S' && ev.px() == -15 && ev.qty() == 3);
  CHECK(ev.has_rec() && ev.rec() == 2);
  CHECK(ev.bids.size() == 1 && ev.bids[0].px == 10 && ev.bids[0].qty == 1);
  CHECK(ev.asks.size() == 2 && ev.asks[1].px == 13 && ev.asks[1].qty == 6);
  CHECK(throws([&] { (void)ev.str("missing"); }));
  CHECK(throws([] { (void)Event::parse("md step=x ts=1"); }));
  CHECK(throws([] { (void)Event::parse("md step=1 ts=1 junk"); }));
  // Numbers are written so the harness reads back the same double as from Python's repr().
  std::string s;
  detail::append_double(s, 0.1);
  CHECK(std::strtod(s.c_str(), nullptr) == 0.1);
  s.clear();
  detail::append_double(s, std::numeric_limits<double>::quiet_NaN());
  CHECK(s == "nan");
  s.clear();
  detail::append_double(s, -std::numeric_limits<double>::infinity());
  CHECK(s == "-inf");
  s.clear();
  detail::append_i64(s, std::numeric_limits<int64_t>::min());
  CHECK(s == "-9223372036854775808");
  // Tokens are validated before anything is written.
  CHECK(detail::valid_token("BRK.B") && detail::valid_token("o-1:2+3_4"));
  CHECK(!detail::valid_token("") && !detail::valid_token("a b") && !detail::valid_token("a=b"));
  CHECK(!detail::valid_token(std::string(65, 'x')));
  // The event loop itself (run) is exercised end to end by the port_check tests.
  if (failures) return 1;
  std::puts("ok");
  return 0;
}
