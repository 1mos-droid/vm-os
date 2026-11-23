#include <stdint.h>
#include <stdbool.h>
#include <string.h>
#include <stdlib.h>
#include <math.h>

#include "monitor_types.h"
#include "monitor.h"

static int64_t nat_spaces_lan_blocked_cpy;
static int64_t nat_spaces_running_cpy;
static int64_t reserved_ram_mib_cpy;
static int64_t ram_budget_mib_cpy;
static int64_t largest_capture_bytes_cpy;
static bool s0[(1)] = {(true)};
static bool s1[(1)] = {(true)};
static bool s2[(1)] = {(true)};
static size_t s0_idx = (0);
static size_t s1_idx = (0);
static size_t s2_idx = (0);

static bool s0_get(size_t x) {
  return (s0)[((s0_idx) + (x)) % ((size_t)(1))];
}

static bool s1_get(size_t x) {
  return (s1)[((s1_idx) + (x)) % ((size_t)(1))];
}

static bool s2_get(size_t x) {
  return (s2)[((s2_idx) + (x)) % ((size_t)(1))];
}

static bool s0_gen(void) {
  return ((nat_spaces_lan_blocked_cpy) == (nat_spaces_running_cpy)) && ((s0_get)((0)));
}

static bool s1_gen(void) {
  return ((reserved_ram_mib_cpy) <= (ram_budget_mib_cpy)) && ((s1_get)((0)));
}

static bool s2_gen(void) {
  return ((largest_capture_bytes_cpy) <= ((int64_t)(268435456))) && ((s2_get)((0)));
}

static bool handlerHYP001_0_guard(void) {
  return !(((nat_spaces_lan_blocked_cpy) == (nat_spaces_running_cpy)) && ((s0_get)((0))));
}

static bool handlerHYP002_1_guard(void) {
  return !(((reserved_ram_mib_cpy) <= (ram_budget_mib_cpy)) && ((s1_get)((0))));
}

static bool handlerHYP003_2_guard(void) {
  return !(((largest_capture_bytes_cpy) <= ((int64_t)(268435456))) && ((s2_get)((0))));
}

void step(void) {
  bool s0_tmp;
  bool s1_tmp;
  bool s2_tmp;
  (nat_spaces_lan_blocked_cpy) = (nat_spaces_lan_blocked);
  (nat_spaces_running_cpy) = (nat_spaces_running);
  (reserved_ram_mib_cpy) = (reserved_ram_mib);
  (ram_budget_mib_cpy) = (ram_budget_mib);
  (largest_capture_bytes_cpy) = (largest_capture_bytes);
  if ((handlerHYP001_0_guard)()) {
    {(handlerHYP001)();}
  };
  if ((handlerHYP002_1_guard)()) {
    {(handlerHYP002)();}
  };
  if ((handlerHYP003_2_guard)()) {
    {(handlerHYP003)();}
  };
  (s0_tmp) = ((s0_gen)());
  (s1_tmp) = ((s1_gen)());
  (s2_tmp) = ((s2_gen)());
  ((s0)[s0_idx]) = (s0_tmp);
  ((s1)[s1_idx]) = (s1_tmp);
  ((s2)[s2_idx]) = (s2_tmp);
  (s0_idx) = (((s0_idx) + ((size_t)(1))) % ((size_t)(1)));
  (s1_idx) = (((s1_idx) + ((size_t)(1))) % ((size_t)(1)));
  (s2_idx) = (((s2_idx) + ((size_t)(1))) % ((size_t)(1)));
}
