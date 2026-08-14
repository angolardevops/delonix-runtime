//! Fechadura exclusiva de ficheiro (`flock`), para serializar as
//! leituras-modificações-escritas que decidem endereços.
//!
//! Existe porque este crate tem MAIS DO QUE UM registo com essa forma — o IPAM,
//! o registo de redes de ingress e o `NetworkStore` — e cada cópia do mesmo
//! `open`+`flock` é mais uma oportunidade de uma delas divergir. O
//! [`crate::ipam`] tem a sua própria (`IpamLock`), anterior a esta e deixada
//! como está de propósito: mexer-lhe é mudar o caminho que entrega endereços a
//! containers vivos, e o comentário dela é a única memória escrita de um bug já
//! corrigido — vale mais lido do que reescrito.
//!
//! **Falível de propósito.** Uma fechadura que guarda unicidade e falha ABERTA é
//! pior do que fechadura nenhuma: quem chama julga-se serializado e não está. Por
//! isso `acquire` devolve `Option` e quem chama tem de RECUSAR a operação —
//! nunca continuar sem ela.

use delonix_runtime_core::Error;
use std::path::Path;

/// Fechadura exclusiva viva enquanto o valor existir. Larga no `Drop`.
pub(crate) struct ExclusiveLock(i32);

impl ExclusiveLock {
    /// Toma a fechadura em `path`, bloqueando até a obter. `None` quando não foi
    /// possível — quem chama TEM de desistir da operação.
    ///
    /// O ficheiro é criado se não existir; o seu conteúdo é irrelevante (só o
    /// `flock` sobre o descritor conta).
    pub(crate) fn acquire(path: &Path) -> Option<ExclusiveLock> {
        if let Some(pai) = path.parent() {
            let _ = std::fs::create_dir_all(pai);
        }
        let c =
            std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec()).ok()?;
        // SAFETY: open com um caminho válido terminado em NUL; -1 é tratado.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return None;
        }
        // SAFETY: o fd é nosso e está aberto; LOCK_EX bloqueia até ser concedida.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            // SAFETY: fechamos o nosso próprio fd, que vamos deitar fora.
            unsafe { libc::close(fd) };
            return None;
        }
        Some(ExclusiveLock(fd))
    }

    /// O erro que quem chama reporta quando não conseguiu a fechadura.
    ///
    /// `consequencia` é o que teria acontecido sem ela, em texto. Nomear a
    /// consequência importa: «não consegui trancar» lê-se como um aborrecimento
    /// passageiro, quando o que evita é duas redes no mesmo `/16`.
    pub(crate) fn unavailable(path: &Path, consequencia: &str) -> Error {
        Error::Runtime {
            context: "lock",
            message: format!(
                "could not lock {} — refusing to continue, since {consequencia}",
                path.display()
            ),
        }
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: fd próprio, aberto em `acquire`.
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
                libc::close(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fechadura_exclui_de_facto() {
        let d = std::env::temp_dir().join(format!("dlx-flock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("f.lock");

        let contador = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barreira = std::sync::Arc::new(std::sync::Barrier::new(8));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (p, c, m, b) = (p.clone(), contador.clone(), max.clone(), barreira.clone());
                std::thread::spawn(move || {
                    b.wait();
                    let _l = ExclusiveLock::acquire(&p).expect("fechadura");
                    // Dentro da secção crítica só pode estar UMA thread: se o
                    // `flock` não excluísse, o contador passaria de 1.
                    let n = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    m.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    c.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn caminho_impossivel_devolve_none_e_nao_falha_aberta() {
        // Um diretório onde o `open` não pode criar nada: a fechadura NÃO se
        // inventa, devolve `None`, e quem chama recusa a operação.
        let p = std::path::Path::new("/proc/1/impossivel/f.lock");
        assert!(ExclusiveLock::acquire(p).is_none());
        let e = ExclusiveLock::unavailable(p, "duas redes podiam ficar no mesmo /16");
        assert!(e.to_string().contains("refusing to continue"));
    }
}
