/*
 * Glue between vmm-daemon and the Copilot runtime monitor generated from
 * assurance/requirements/host.json (see assurance/tools/generate.sh).
 *
 * The generated code reads its inputs from the extern variables below on every
 * step() and calls handlerHYP00x() when a requirement is violated. This shim
 * owns those symbols and exposes a single call to Rust. The monitor keeps
 * static state, so callers must serialise hyperos_monitor_sample().
 */
#include <stdint.h>
#include "generated/monitor.h"

/* Monitor inputs (names and types fixed by host.json). */
int64_t nat_spaces_running;
int64_t nat_spaces_lan_blocked;
int64_t reserved_ram_mib;
int64_t ram_budget_mib;
int64_t largest_capture_bytes;

/* Must match `HyperosSample` in src/assurance.rs field for field. */
struct hyperos_sample {
    int64_t nat_spaces_running;
    int64_t nat_spaces_lan_blocked;
    int64_t reserved_ram_mib;
    int64_t ram_budget_mib;
    int64_t largest_capture_bytes;
};

static uint32_t violations;

/* Bit order must match REQUIREMENTS in src/assurance.rs. */
void handlerHYP001(void) { violations |= UINT32_C(1) << 0; }
void handlerHYP002(void) { violations |= UINT32_C(1) << 1; }
void handlerHYP003(void) { violations |= UINT32_C(1) << 2; }

uint32_t hyperos_monitor_sample(const struct hyperos_sample *s)
{
    nat_spaces_running = s->nat_spaces_running;
    nat_spaces_lan_blocked = s->nat_spaces_lan_blocked;
    reserved_ram_mib = s->reserved_ram_mib;
    ram_budget_mib = s->ram_budget_mib;
    largest_capture_bytes = s->largest_capture_bytes;
    violations = 0;
    step();
    return violations;
}
