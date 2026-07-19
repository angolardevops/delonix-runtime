//! Typestate do ciclo de vida de um container (Sprint 5 — Damas: *correção por
//! construção*). Os estados são **tipos**, e as transições ilegais **não
//! compilam** — em vez de serem apanhadas (ou não) em runtime por um `match` sobre
//! um [`Status`](crate::Status).
//!
//! O modelo: `Created → Running → Stopped → (restart) Created`. Cada transição
//! **consome** a fase anterior, por isso uma fase obsoleta não pode ser reutilizada.
//!
//! ```
//! use delonix_runtime_core::typestate::Phase;
//! use delonix_runtime_core::Status;
//!
//! let created = Phase::new("abc123");            // Phase<Created>
//! assert_eq!(created.status(), Status::Created);
//! let running = created.start(4242);             // Created → Running
//! assert_eq!(running.pid(), 4242);
//! let stopped = running.stop(0);                 // Running → Stopped
//! assert_eq!(stopped.status(), Status::Stopped);
//! let _again = stopped.restart();                // Stopped → Created (reusa o id)
//! ```
//!
//! As transições inválidas são **erros de compilação**, não bugs em runtime:
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let created = Phase::new("abc123"); // Phase<Created>
//! created.stop(0);                    // ERRO: `stop` só existe em Phase<Running>
//! ```
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let running = Phase::new("abc123").start(1); // Phase<Running>
//! running.start(2);                            // ERRO: `start` só existe em Phase<Created>
//! ```
//!
//! ```compile_fail
//! use delonix_runtime_core::typestate::Phase;
//! let created = Phase::new("abc123");
//! let _running = created.start(1);
//! created.start(2);                  // ERRO: `created` foi consumido pela 1.ª transição
//! ```

use crate::Status;
use std::marker::PhantomData;

/// Estado: criado, ainda sem `pid`.
pub struct Created;
/// Estado: em execução, com um `pid` de init vivo.
pub struct Running;
/// Estado: terminado, com um código de saída.
pub struct Stopped;

/// Uma fase **tipada** do ciclo de vida. O parâmetro `S` é o estado atual; os
/// métodos de transição só existem nas fases onde são válidos.
pub struct Phase<S> {
    id: String,
    pid: Option<i32>,
    code: Option<i32>,
    _state: PhantomData<S>,
}

impl<S> Phase<S> {
    /// O identificador do container (estável ao longo das transições).
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Phase<Created> {
    /// Cria uma fase nova no estado `Created`.
    pub fn new(id: impl Into<String>) -> Self {
        Phase {
            id: id.into(),
            pid: None,
            code: None,
            _state: PhantomData,
        }
    }
    /// `Created → Running`. Consome a fase (a anterior deixa de ser utilizável).
    pub fn start(self, pid: i32) -> Phase<Running> {
        Phase {
            id: self.id,
            pid: Some(pid),
            code: None,
            _state: PhantomData,
        }
    }
    /// O [`Status`](crate::Status) correspondente.
    pub fn status(&self) -> Status {
        Status::Created
    }
}

impl Phase<Running> {
    /// O `pid` do init (existe só no estado `Running`).
    pub fn pid(&self) -> i32 {
        self.pid.expect("Phase<Running> tem sempre pid")
    }
    /// `Running → Stopped`, guardando o código de saída.
    pub fn stop(self, code: i32) -> Phase<Stopped> {
        Phase {
            id: self.id,
            pid: None,
            code: Some(code),
            _state: PhantomData,
        }
    }
    /// O [`Status`](crate::Status) correspondente.
    pub fn status(&self) -> Status {
        Status::Running
    }
}

impl Phase<Stopped> {
    /// O código de saída (existe só no estado `Stopped`).
    pub fn exit_code(&self) -> i32 {
        self.code.expect("Phase<Stopped> tem sempre código")
    }
    /// `Stopped → Created` (restart): reutiliza o id, limpa pid/código.
    pub fn restart(self) -> Phase<Created> {
        Phase {
            id: self.id,
            pid: None,
            code: None,
            _state: PhantomData,
        }
    }
    /// O [`Status`](crate::Status) correspondente: código 0 → Stopped, ≠0 → Failed.
    pub fn status(&self) -> Status {
        match self.code.unwrap_or(0) {
            0 => Status::Stopped,
            n => Status::Failed(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lifecycle_preserves_id_and_maps_status() {
        let c = Phase::new("deadbeef");
        assert_eq!(c.id(), "deadbeef");
        assert_eq!(c.status(), Status::Created);

        let r = c.start(99);
        assert_eq!(r.id(), "deadbeef");
        assert_eq!(r.pid(), 99);
        assert_eq!(r.status(), Status::Running);

        let s = r.stop(137);
        assert_eq!(s.id(), "deadbeef");
        assert_eq!(s.exit_code(), 137);
        assert_eq!(s.status(), Status::Failed(137));

        let again = s.restart();
        assert_eq!(again.id(), "deadbeef");
        assert_eq!(again.status(), Status::Created);
    }
}
