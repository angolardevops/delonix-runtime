//! Handlers dos re-exec **mapeados** (`__rmtree`, `__volsnap`) — as metades que
//! faltavam ao contrato do próprio motor.
//!
//! # Porque isto existe
//!
//! Em rootless com subuid, os ficheiros escritos por um container pertencem a
//! uids **mapeados** (ex.: uid 0 do container → 100000 no host). O utilizador
//! real não os consegue apagar nem ler. A solução (a mesma do `podman unshare`):
//! `delonix-runtime` faz fork de um filho num user namespace, mapeia-lhe o
//! intervalo de subuid com `newuidmap`, e o filho — já root NESSE userns, logo
//! dono efectivo dos subuids — re-executa `delonix __rmtree <path>` ou
//! `delonix __volsnap <modo> <data> <tarball>`.
//!
//! **O contrato estava meio implementado no repo público**: a biblioteca
//! (`delonix_runtime::{remove_tree_mapped, reexec_mapped}`) fazia o re-exec, mas
//! os subcomandos só existiam na CLI PRIVADA do `delonix-paas`. Um utilizador do
//! `delonix` público apanhava o filho a morrer com "unrecognized subcommand
//! '__rmtree'" (rc=2) — e como o `remove_tree_mapped` nem sequer olhava para o
//! exit status, a árvore ficava por apagar **em silêncio**. Verificado a correr:
//! `delonix __rmtree /x` → rc=2.
//!
//! Não são subcomandos públicos: o `main` intercepta-os antes do clap (como o
//! `netns holder`), e o utilizador nunca os invoca à mão.

use std::path::Path;

use delonix_runtime_core::{Error, Result};

fn io_err(context: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e: std::io::Error| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// `__rmtree <path>` — apaga uma árvore inteira, incluindo ficheiros de subuid.
///
/// Já corremos como root num userns mapeado (o pai usou `newuidmap`), por isso um
/// `remove_dir_all` normal chega: dentro deste userns somos donos dos subuids.
pub fn rmtree(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).or_else(|e| {
        // Já não existir é sucesso — o objectivo é "não estar lá".
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(io_err("__rmtree")(e))
        }
    })
}

/// `__volsnap create <data> <tarball>` — tar.gz do `_data` de um volume.
///
/// Escreve num `.tmp` e faz `rename`: um crash a meio não deixa um snapshot
/// truncado a fingir-se de bom.
pub fn volsnap_create(data: &Path, tarball: &Path) -> Result<()> {
    if let Some(dir) = tarball.parent() {
        std::fs::create_dir_all(dir).map_err(io_err("volume snapshot"))?;
    }
    let tmp = tarball.with_extension("tar.gz.tmp");
    let f = std::fs::File::create(&tmp).map_err(io_err("volume snapshot"))?;
    let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    let mut b = tar::Builder::new(enc);
    b.follow_symlinks(false); // symlinks entram como symlinks, não o alvo
    b.append_dir_all(".", data)
        .map_err(io_err("volume snapshot"))?;
    b.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(io_err("volume snapshot"))?;
    std::fs::rename(&tmp, tarball).map_err(io_err("volume snapshot"))?;
    Ok(())
}

/// `__volsnap restore <data> <tarball>` — repõe o `_data` a partir do tar.gz.
///
/// Limpa o CONTEÚDO e não o próprio `_data` (mantém o inode/mountpoint: pode
/// estar montado num container a correr). Donos e permissões preservados — no
/// userns mapeado o chown dos subuids funciona.
pub fn volsnap_restore(data: &Path, tarball: &Path) -> Result<()> {
    let f = std::fs::File::open(tarball).map_err(io_err("volume restore"))?;
    for e in std::fs::read_dir(data).map_err(io_err("volume restore"))? {
        let p = e.map_err(io_err("volume restore"))?.path();
        if p.is_dir() && !p.is_symlink() {
            std::fs::remove_dir_all(&p).map_err(io_err("volume restore"))?;
        } else {
            std::fs::remove_file(&p).map_err(io_err("volume restore"))?;
        }
    }
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    a.set_preserve_permissions(true);
    a.set_preserve_ownerships(true);
    a.set_overwrite(true);
    a.unpack(data).map_err(io_err("volume restore"))?;
    Ok(())
}

/// `__buildtar <rootfs> <out>` — empacota um rootfs FLAT (build rootless) num
/// tar NÃO-comprimido, corrido DENTRO do userns mapeado.
///
/// Porque mapeado: um `RUN` com `apt-get install` (dpkg) deixa ficheiros de
/// subuid com modos restritos (`/var/cache/ldconfig/aux-cache` 0600, dirs
/// `.../partial` 0700). O `commit_flat_rootfs` a empacotar como utilizador REAL
/// não os consegue ler → `Permission denied` e o build inteiro falha no fim
/// (depois de todos os RUN passarem — o pior sítio para falhar). Aqui somos root
/// no userns (donos efectivos dos subuids), logo lemos tudo; e a tar regista uid
/// 0, não o número do subuid — mais correcto para um layer OCI.
///
/// Tar NÃO-comprimido de propósito: o `commit_flat_rootfs_from_tar` usa o digest
/// deste tar como `diff_id` (o OCI exige o digest do tar DEScomprimido). O `out`
/// fica com modo legível a todos (0644) para o pai — que não é dono do subuid —
/// o conseguir ler de volta.
pub fn buildtar(rootfs: &Path, out: &Path) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(io_err("build tar"))?;
    }
    let f = std::fs::File::create(out).map_err(io_err("build tar"))?;
    let mut b = tar::Builder::new(f);
    b.follow_symlinks(false);
    b.append_dir_all(".", rootfs).map_err(io_err("build tar"))?;
    b.finish().map_err(io_err("build tar"))?;
    Ok(())
}

/// Despacha `__volsnap <modo> <data> <tarball>`.
pub fn volsnap(mode: &str, data: &Path, tarball: &Path) -> Result<()> {
    match mode {
        "create" => volsnap_create(data, tarball),
        "restore" => volsnap_restore(data, tarball),
        other => Err(Error::Invalid(format!(
            "__volsnap: modo desconhecido '{other}' (create|restore)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(nome: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("delonix-mapped-{nome}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rmtree_apaga_a_arvore() {
        let d = tmpdir("rm");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/f"), b"x").unwrap();
        rmtree(&d).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn rmtree_e_idempotente() {
        // O objectivo é "não estar lá" — apagar o que já não existe é sucesso,
        // senão um `container rm` repetido falhava sem razão.
        let d = tmpdir("rm-idem");
        std::fs::remove_dir_all(&d).unwrap();
        rmtree(&d).unwrap();
    }

    #[test]
    fn volsnap_round_trip_preserva_conteudo() {
        let base = tmpdir("snap");
        let data = base.join("_data");
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::write(data.join("sub/ficheiro"), b"conteudo").unwrap();
        let tar = base.join("_snapshots/s1.tar.gz");

        volsnap_create(&data, &tar).unwrap();
        assert!(tar.exists(), "o snapshot devia existir");
        // Sem .tmp deixado para trás.
        assert!(!tar.with_extension("tar.gz.tmp").exists());

        // Mexe no _data e repõe.
        std::fs::write(data.join("sub/ficheiro"), b"estragado").unwrap();
        std::fs::write(data.join("intruso"), b"a apagar").unwrap();
        volsnap_restore(&data, &tar).unwrap();

        assert_eq!(
            std::fs::read(data.join("sub/ficheiro")).unwrap(),
            b"conteudo"
        );
        assert!(
            !data.join("intruso").exists(),
            "o restore tem de limpar o que não estava no snapshot"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn volsnap_restore_mantem_o_proprio_data() {
        // O `_data` pode estar montado num container vivo: limpa-se o conteúdo,
        // nunca o directório (senão o mount ficava a apontar para um inode morto).
        let base = tmpdir("snap-inode");
        let data = base.join("_data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("f"), b"v1").unwrap();
        let tar = base.join("s.tar.gz");
        volsnap_create(&data, &tar).unwrap();
        let ino_antes = std::fs::metadata(&data).unwrap().rt_ino();
        volsnap_restore(&data, &tar).unwrap();
        assert_eq!(
            ino_antes,
            std::fs::metadata(&data).unwrap().rt_ino(),
            "o inode do _data mudou"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn buildtar_empacota_o_rootfs() {
        let base = tmpdir("buildtar");
        let rootfs = base.join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/hostname"), b"delonix").unwrap();
        std::fs::write(rootfs.join("app"), b"bin").unwrap();
        let out = base.join("layer.tar");

        buildtar(&rootfs, &out).unwrap();
        assert!(out.exists(), "o tar devia existir");

        // O tar contém as entradas do rootfs (verifica re-lendo).
        let mut a = tar::Archive::new(std::fs::File::open(&out).unwrap());
        let mut nomes: Vec<String> = a
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        nomes.sort();
        assert!(
            nomes.iter().any(|n| n.ends_with("etc/hostname")),
            "faltou etc/hostname: {nomes:?}"
        );
        assert!(
            nomes.iter().any(|n| n.ends_with("app")),
            "faltou app: {nomes:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn volsnap_modo_invalido_e_erro_claro() {
        let d = tmpdir("snap-modo");
        let err = volsnap("destruir", &d, &d.join("t.tar.gz")).unwrap_err();
        assert!(
            format!("{err}").contains("modo desconhecido"),
            "erro pouco claro: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    trait RtIno {
        fn rt_ino(&self) -> u64;
    }
    impl RtIno for std::fs::Metadata {
        fn rt_ino(&self) -> u64 {
            use std::os::unix::fs::MetadataExt;
            self.ino()
        }
    }
}
