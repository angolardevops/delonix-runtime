# 46/C2 — Perfil de tempo da criação de um cluster

| Campo | Valor |
|---|---|
| Data | 2026-08-10 |
| Comando | `delonix cluster create --name perfc2 --workers 0` (modo kind, 1 control-plane) |
| Host | rootless, 5 containers de produção a correr, sob `systemd-run --user --scope -p Delegate=yes` |
| Imagem | `kindest/node:v1.34.0` (424.8 MiB), **já local** — o `pull` não entra em nenhum destes números |
| Método | Uma corrida real, com cada linha de progresso carimbada com o tempo desde o arranque |

---

## 0. A premissa do enunciado está errada, e é preciso dizê-lo

O pedido original fala em ter «um cluster Kubernetes no ar em alguns **milissegundos**». Isso
é fisicamente impossível, e não por falta de optimização: o `kubeadm init` **espera** que o
etcd, o apiserver, o controller-manager e o scheduler fiquem saudáveis, e um nó kind arranca
`systemd` e `containerd` por dentro antes de qualquer disso começar. Nada que este motor faça
reduz uma espera por *readiness* de outro processo a milissegundos.

O que se pode fazer — e é o que este documento entrega — é **medir onde está o tempo**,
encurtar o que é nosso, e declarar o piso.

## 1. O perfil medido

```
    0.0s  ✓ Ensuring node image (kindest/node:v1.34.0)
    2.6s  (container do nó criado)
   79.4s  ✓ Preparing nodes (1)
   79.4s  ✓ Writing configuration
  541.7s  ✓ Starting control-plane
  542.0s  ✓ Installing CNI (kindnet)
  560.5s  ✓ Waiting for control-plane to be Ready
  560.5s  kubeconfig escrito, contexto definido
```

| Etapa | Duração | Fatia | De quem é o tempo |
|---|---|---|---|
| Garantir a imagem do nó | ~0 s | — | nosso (já estava local) |
| Criar o container do nó | 2.6 s | 0.5 % | **nosso** |
| Preparar o nó | 76.8 s | 14 % | misto: o nosso `run` + `systemd`/`containerd` a arrancar lá dentro |
| Escrever a configuração | ~0 s | — | nosso |
| **`kubeadm init`** | **462.3 s** | **82 %** | **do kubeadm** |
| Instalar o CNI | 0.3 s | 0.05 % | nosso (aplica um manifesto) |
| Esperar `Ready` | 18.5 s | 3 % | do kubelet/CNI |
| **Total** | **560.5 s** | | |

**O `kubeadm init` domina com 82 %.** Tudo o que o motor controla — criar o container, escrever
a configuração, aplicar o CNI — soma menos de 3 segundos em 560.

## 2. O ganho real obtido, e porque é pequeno AQUI

A correcção de performance desta mesma sessão (o rootfs deixar de ser extraído duas vezes em
`--net <rede-custom>`, ver o commit `perf:`) aplica-se a cada nó, porque um nó kind **é** um
`container run` numa rede custom. Medido com a própria imagem de nó:

| | `container run` de `kindest/node` |
|---|---|
| antes | 5 226 ms |
| depois | **2 762 ms** (−48 %) |

Mas é preciso ser honesto sobre o que isso vale no cluster: **~2.5 s poupados em 560 s = 0.45 %**.
Cheguei a sugerir, antes de medir a etapa, que a correcção encurtaria a criação de clusters de
forma sensível. **Não encurta.** O `container run` do nó são ~2.7 s de uma etapa de 76.8 s, e
essa etapa é 14 % de um total dominado pelo `kubeadm`. A correcção é excelente para
`container run` (−46 % numa imagem de 431 MB) e quase irrelevante para `cluster create`.

Num cluster de N nós o ganho escala (~2.5 s por nó), e continua pequeno ao lado dos 462 s.

## 3. O piso teórico, declarado

Com a imagem do nó já local e o motor a custar <3 s, o piso é **o que o kubeadm e o kubelet
demoram a ficar saudáveis**. Nesta máquina isso foi 462 s + 18 s. Um kind típico faz o mesmo
trabalho em 30–60 s, o que diz que este host está muito abaixo do normal — provavelmente
contenção de CPU (5 containers de produção a correr, mais builds de Rust em paralelo durante a
medição). **Não isolei o interior dos 462 s** e digo-o em vez de o atribuir: fazê-lo exige
outra corrida com verbosidade do `kubeadm`, ou seja mais ~9 minutos, e não a fiz.

O que se sabe do interior, e é pouco: as imagens do control-plane **estão** no containerd do
nó (`kube-apiserver-amd64`, `etcd`, `coredns` — o conjunto que o `kindest/node` traz), e todos
os componentes só apareceram a correr perto do FIM da janela. Portanto os ~7 minutos foram
gastos **antes** de os static pods subirem, não a descarregar imagens.

## 4. Candidatos, e o que NÃO vale a pena

- **Não vale a pena** optimizar a criação do container do nó, escrever a configuração ou
  aplicar o CNI: somados dão menos de 3 s. Optimizar 0.5 % é ruído.
- **Vale a pena investigar** o interior dos 462 s antes de qualquer outra coisa — é 82 % do
  problema, e sem saber se é contenção deste host ou algo estrutural, qualquer trabalho a
  jusante é palpite. É a próxima medição, não a próxima correcção.
- **Paralelizar nós** (para `--workers N`) só ataca a etapa de 76.8 s e só quando N > 1; o
  `kubeadm init` do primeiro control-plane é sequencial por natureza, e os `join` dependem do
  token que ele produz.

## 5. Higiene

O cluster `perfc2` foi removido no fim. O host ficou com os 5 containers de produção e nada
mais.
