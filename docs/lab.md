# O laboratório nocturno

Decisão D1 de [`docs/discovery/65_PLANO_MATURIDADE.md`](discovery/65_PLANO_MATURIDADE.md).

O runner alojado do GitHub bloqueia user namespaces sem privilégio. Por isso o
workflow de caos fica `skipped` e a bateria E2E nunca corre no CI. O workflow
[`.github/workflows/lab.yml`](../.github/workflows/lab.yml) corre tudo todas as
noites num runner self-hosted:

1. o preflight do host;
2. o build;
3. a bateria E2E inteira;
4. o gate dos SKIPs;
5. o arnês de caos;
6. o gate de performance.

## O host

O que o `scripts/lab_preflight.sh` exige. Ele nomeia TUDO o que falta e sai com
erro:

| Peça | Porque |
|---|---|
| user namespaces sem privilégio, `/etc/subuid` e `/etc/subgid` | o motor é rootless |
| `cpu cpuset io memory pids` delegados ao `user@<uid>.service` | sem eles os limites são recusados, e a bateria salta-os |
| `br_netfilter` carregado e `bridge-nf-call-iptables=1` | sem eles o isolamento por namespace é recusado (D5) |
| `/dev/kvm` com leitura e escrita; `virsh`, `qemu-*`, `cloud-hypervisor`, `cloud-localds`, `virt-customize`; `qemu:///system` acessível | as secções de VM dos dois backends locais |
| o EDK2 `CLOUDHV.fd` em `/usr/local/share/delonix` | é o único firmware que arranca as imagens do projecto em Cloud Hypervisor |
| `slirp4netns`, `nft`, `ip`, `conntrack`, `newuidmap`, `protoc`, `wg` | rede rootless, overlay cifrado e build |
| 80 GiB livres na home do runner | imagens, VMs e o target do cargo |

A delegação completa dos cinco controladores exige um drop-in como root:

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload && sudo systemctl restart user@$(id -u runner).service
```

Só o `daemon-reload` não chega, porque o `user@` já a correr não muda de
controladores. Está medido no `AGENTS.md`.

## Registar o runner

Num host que passe o preflight, com uma conta própria (`runner`), sem sudo e com
linger activo:

```bash
sudo loginctl enable-linger runner
```

A seguir, em *Settings → Actions → Runners → New self-hosted runner* do
repositório, segue as instruções do GitHub e dá ao runner a etiqueta
**`delonix-lab`**. Instala-o como serviço da conta `runner`.

Confirma com um disparo manual:

```bash
gh workflow run lab.yml
```

## Os SKIPs declarados

`scripts/lab_allowed_skips.txt` começa vazio. Depois da primeira noite com o host
completo, imprime os SKIPs que sobraram:

```bash
python3 scripts/lab_skip_gate.py --print /tmp/delonix-lab-e2e/results.jsonl
```

Cada um que fique na lista leva a razão pela qual o laboratório não consegue
fazer essa medição: credenciais de um provider remoto, uma GPU, ou um segundo
nó. O gate falha também quando uma linha declarada passa a correr. Nesse caso a
linha sai no mesmo commit.
