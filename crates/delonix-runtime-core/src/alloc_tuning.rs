//! Allocator tuning for the processes that OUTLIVE a command.
//!
//! # The measurement
//!
//! `delonix serve api` on a 32-core host, with the published binary, three
//! runs differing only in one environment variable:
//!
//! ```text
//! MALLOC_ARENA_MAX   VmSize        VmRSS      threads   64 MiB arenas
//! (default)          2 329 176 kB  13 272 kB     34          34
//! =2                   166 488 kB  13 140 kB     34           1
//! =1                   100 948 kB  12 976 kB     34           0
//! ```
//!
//! glibc gives a thread that contends on the main arena a fresh 64 MiB heap of
//! its own, and caps the count at `8 × ncores` — 256 here, so nothing stops it
//! before the thread count does. A server with one worker per core therefore
//! reserves gigabytes it will never touch. The `netns control` running on the
//! host this was written against showed the same shape in miniature: **637 MB
//! of `VmSize` and nine arenas for 2.3 MB of RSS**.
//!
//! # Why it is worth a call
//!
//! The reservations are `PROT_NONE`, so this is NOT resident memory and the RSS
//! column above is the proof — it does not move. Three things it does cost:
//!
//! 1. **Commit charge.** Under `vm.overcommit_memory=2` the kernel accounts the
//!    reservation, and 2.3 GB per server is the difference between a node that
//!    starts its services and one that does not.
//! 2. **VMAs and page tables**, one entry per arena per process, walked on fork
//!    and on teardown.
//! 3. **The number an operator reads.** Every monitor, this engine's own
//!    dashboard included, shows `VmSize`. Twenty times the truth is not a
//!    cosmetic problem when somebody is deciding whether a node is out of
//!    memory.
//!
//! # Why 2 and not 1
//!
//! One arena serialises every allocating thread on a single lock. These are
//! I/O-bound servers, so the contention is small either way — but 2 keeps a
//! second thread off the critical path for a further 64 MiB of address space,
//! which is not a trade worth taking the risk for.
//!
//! # Why this is a call and not a `[profile]` setting
//!
//! Replacing the global allocator (`mimalloc`, `jemalloc`) would achieve the
//! same and more, at the cost of a new dependency in the supply chain of a
//! public container runtime. `libc` is already a dependency of this crate, and
//! `mallopt` is one function of it.
//!
//! # Why it is NOT called from `main`
//!
//! A short-lived CLI command has nothing to gain — it exits before the arenas
//! matter — and a few of them (a parallel image pull, a multi-stage build) do
//! allocate hard across threads, where fewer arenas is the wrong trade. This is
//! called explicitly by the processes that stay: see the callers of
//! [`limit_malloc_arenas`].

/// Caps the number of glibc malloc arenas for a process that will stay alive.
///
/// Call it ONCE, as early in the process as possible: `mallopt` only governs
/// arenas created after it, so an arena a thread already took keeps its
/// reservation. Idempotent and harmless if called twice.
///
/// Silent on failure by design. This is an optimisation, not a precondition —
/// a `mallopt` that returns 0 (or a libc without it) means the process keeps
/// the address space it would have had anyway, which is exactly today's
/// behaviour and never a reason to refuse to serve.
pub fn limit_malloc_arenas() {
    #[cfg(target_env = "gnu")]
    {
        // An explicit `MALLOC_ARENA_MAX` in the environment is the operator
        // speaking, and glibc reads it before we get here. Overriding it would
        // make an escape hatch that does not escape.
        if std::env::var_os("MALLOC_ARENA_MAX").is_some() {
            return;
        }
        // SAFETY: `mallopt` takes two ints and touches no memory of ours.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

/// Returns the arenas' unused pages to the kernel.
///
/// For a process that allocates in bursts and then idles — the control plane
/// after a wave of attaches, a server after a build — glibc holds the freed
/// pages in its arenas rather than returning them, so RSS stays at the high
/// water mark for the life of the process. This asks for them back.
///
/// Deliberately NOT wired to a timer here: when to call it is the caller's
/// judgement (it walks the arenas, so it is not free), and a caller that never
/// calls it behaves exactly as it does today.
pub fn release_free_memory() {
    #[cfg(target_env = "gnu")]
    // SAFETY: `malloc_trim` takes a size and touches no memory of ours.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    /// The guard exists so an operator can still say otherwise. Without it the
    /// escape hatch documented in the module comment would be inert — the kind
    /// of accepted-and-ignored option this repo removes wherever it finds one.
    #[test]
    fn um_valor_explicito_do_operador_nao_e_ultrapassado() {
        // Cannot assert on glibc's internal state from here; what IS assertable
        // is that the function returns without touching anything when the
        // variable is set, and that it is safe to call in both states.
        std::env::set_var("MALLOC_ARENA_MAX", "16");
        super::limit_malloc_arenas();
        std::env::remove_var("MALLOC_ARENA_MAX");
        super::limit_malloc_arenas();
        super::release_free_memory();
    }
}
