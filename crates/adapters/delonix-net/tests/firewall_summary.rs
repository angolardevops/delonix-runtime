//! O resumo de firewall reposto depois de o `Net` sair do motor.
//!
//! Testa o PARSER, não o holder: a leitura em si é um verbo read-only do
//! protocolo de controlo e exige um netns de pé; o que erra é a interpretação
//! das linhas do `nft`, e isso testa-se com listagens reais.

use delonix_net::infra::{parse_dnat, parse_set_elements};

/// Uma listagem `nft list chain` como o holder a devolve, com o prefixo de
/// handle que o `-a` acrescenta.
const CADEIA: &str = r#"
table ip dlxing {
	chain pre {
		type nat hook prerouting priority dstnat; policy accept;
		iif "tap0" tcp dport 8080 dnat to 10.200.0.5:80 # handle 12
		iif "tap0" udp dport 5353 dnat to 10.200.0.9:53 # handle 13
		iif "tap0" tcp dport 5432 dnat to 10.200.0.20:5432 # handle 14
		ct state established,related accept # handle 15
	}
}
"#;

#[test]
fn extrai_as_publicacoes_e_o_protocolo_certo() {
    let r = parse_dnat(CADEIA);
    assert_eq!(r.len(), 3, "devia apanhar as três regras de dnat: {r:?}");

    assert_eq!(r[0].proto, "tcp");
    assert_eq!(r[0].host_port, "8080");
    assert_eq!(r[0].to, "10.200.0.5:80");

    // O UDP é o caso que a versão anterior errava: ela lia o protocolo do INÍCIO
    // da linha (`starts_with("udp")`), e numa listagem indentada — ou com o
    // prefixo do `-a` — o início é espaço ou `iif`, portanto dava `tcp` a tudo.
    // Um resumo que troca UDP por TCP manda quem depura procurar o problema no
    // protocolo errado.
    assert_eq!(
        r[1].proto, "udp",
        "o protocolo lê-se do match, não do início"
    );
    assert_eq!(r[1].host_port, "5353");
    assert_eq!(r[1].to, "10.200.0.9:53");

    assert_eq!(r[2].to, "10.200.0.20:5432");
}

#[test]
fn linhas_sem_dnat_sao_ignoradas() {
    // A cadeia tem regras que não são publicações (`ct state … accept`), e a
    // política. Nenhuma pode aparecer como porto publicado.
    assert!(parse_dnat("ct state established,related accept").is_empty());
    assert!(parse_dnat("type nat hook prerouting priority dstnat; policy accept;").is_empty());
    assert!(parse_dnat("").is_empty());
}

#[test]
fn regra_truncada_nao_vira_publicacao_meia() {
    // Sem porto, ou sem destino, não há regra — melhor não mostrar do que
    // mostrar um porto publicado para lado nenhum.
    assert!(parse_dnat("iif \"tap0\" tcp dnat to 10.200.0.5:80").is_empty());
    assert!(parse_dnat("iif \"tap0\" tcp dport 8080 dnat to ").is_empty());
}

#[test]
fn extrai_os_bloqueados_do_conjunto() {
    let set = r#"
table ip dlxing {
	set blocked {
		type ipv4_addr
		elements = { 10.200.0.7, 10.200.0.9 }
	}
}
"#;
    assert_eq!(parse_set_elements(set), vec!["10.200.0.7", "10.200.0.9"]);
}

#[test]
fn conjunto_vazio_ou_ausente_da_lista_vazia() {
    // Um conjunto sem elementos é a forma NORMAL de «ninguém bloqueado» — não
    // pode ser confundido com uma leitura falhada.
    assert!(
        parse_set_elements("table ip dlxing {\n\tset blocked {\n\t\ttype ipv4_addr\n\t}\n}")
            .is_empty()
    );
    assert!(parse_set_elements("").is_empty());
}
