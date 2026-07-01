//! `delonix-image` — imagens OCI do Delonix Engine.
//!
//! Junta as quatro peças do Mês 5 (Parte B): **CAS** ([`cas`]), **modelo +
//! armazém** ([`image`]), **load** ([`load`]), **overlay2** ([`overlay`]) e
//! **build** ([`build`]).

pub mod auth;
pub mod build;
pub mod buildpack;
pub mod cas;
pub mod detect;
pub mod image;
pub mod internal_registry;
pub mod load;
pub mod overlay;
pub mod registry;
pub mod sign;

pub use buildpack::CnbPlan;
pub use cas::{sha256_hex, Cas};
pub use detect::{detect, Detected};
pub use image::{Image, ImageConfig, ImageStore};
pub use load::load_docker_archive;
pub use registry::{
    build_manifest, http_get, http_post_json, http_post_stream, pull_from_registry, push_to_registry,
};
pub use sign::verify_signature;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cas_write_read_dedup_verify() {
        let dir = std::env::temp_dir().join(format!("delonix-cas-{}", sha256_hex(b"t")));
        let cas = Cas::open(&dir).unwrap();
        let d1 = cas.write(b"layer-data").unwrap();
        let d2 = cas.write(b"layer-data").unwrap();
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
        assert_eq!(cas.read(&d1).unwrap(), b"layer-data");
        assert!(cas.verify(&d1).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dockerfile_parses_from_run_cmd() {
        let df = build::parse_dockerfile(
            "# comment\nFROM alpine:3.19\nRUN echo hi > /a.txt\nCMD [\"/bin/sh\"]\n",
        )
        .unwrap();
        assert_eq!(df.from, "alpine:3.19");
        assert_eq!(df.steps.len(), 1);
        assert!(matches!(&df.steps[0], build::Step::Run(c) if c == "echo hi > /a.txt"));
        assert_eq!(df.cmd, vec!["/bin/sh"]);
    }

    #[test]
    fn dockerfile_rejects_unknown_instruction() {
        assert!(build::parse_dockerfile("FROM x\nWEIRD y").is_err());
        assert!(build::parse_dockerfile("RUN nothing").is_err());
    }

    #[test]
    fn normalise_tag_adds_latest() {
        assert_eq!(image::normalise_tag("alpine"), "alpine:latest");
        assert_eq!(image::normalise_tag("alpine:3.19"), "alpine:3.19");
    }

    #[test]
    fn retag_move_a_tag_e_nao_a_duplica() {
        use image::{Image, ImageConfig, ImageStore};
        let root = std::env::temp_dir().join(format!("delonix-tagtest-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let store = ImageStore::open(&root).unwrap();
        let mk = |id: &str, tag: &str| Image {
            id: format!("sha256:{id}"),
            repo_tags: vec![tag.to_string()],
            layers: vec![],
            config: ImageConfig::default(),
            created_unix: 1,
        };
        let a = mk(&"a".repeat(64), "app:latest");
        let b = mk(&"b".repeat(64), "other:latest");
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        // re-etiquetar B com a tag de A: deve MOVER (A fica sem ela; B passa a tê-la).
        store.tag("other:latest", "app:latest").unwrap();
        let resolved = store.resolve("app:latest").unwrap();
        assert_eq!(resolved.id, b.id, "app:latest devia agora apontar para B");
        // só UMA imagem tem app:latest.
        let holders = store
            .list()
            .unwrap()
            .into_iter()
            .filter(|i| i.repo_tags.iter().any(|t| t == "app:latest"))
            .count();
        assert_eq!(holders, 1, "a tag não pode apontar para duas imagens");
        std::fs::remove_dir_all(&root).ok();
    }
}
