# actions-rust

[![CI](https://github.com/vcyber-tech/actions-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/vcyber-tech/actions-rust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
Coleção de ferramentas Rust e GitHub Actions para pipelines de CI/CD.

Cada ferramenta é um binário estático (musl) empacotado como GitHub Action
composite, seguro para uso em runners Linux x64.

## Ações disponíveis

| Ação | Descrição | Status |
|---|---|---|
| [`vpn-setup`](./tools/actions/vpn-setup) | Estabelece túnel VPN com healthcheck TCP real | ✅ estável |
| [`vpn-teardown`](./tools/actions/vpn-teardown) | Encerra túnel VPN e limpa arquivos de trabalho | ✅ estável |

## Uso rápido / Como chamar nos pipelines

```yaml
steps:
  - name: Checagem Repositório
    uses: actions/checkout@v7

  - name: VPN Setup
    id: vpn
    uses: vcyber-tech/actions-rust/tools/actions/vpn-setup@vpn-setup/v1
    with:
      config: ${{ secrets.VPN_CONFIG_INLINE }}
      healthcheck-host: internal.dns.example
      healthcheck-port: '53'

  - name: Deploy
    run: |
      echo "Túnel em ${{ steps.vpn.outputs.interface }} (${{ steps.vpn.outputs.tunnel-ip }})"

  - name: VPN Teardown
    if: always()
    uses: vcyber-tech/actions-rust/tools/actions/vpn-teardown@vpn-teardown/v1
```

## Estrutura do repositório

```yaml
tools/                         # workspace Rust
├── crates/
│   ├── toolcore/              # biblioteca compartilhada (utilitários do GH Actions)
│   └── vpnctl/                # binário da VPN
└── actions/
    ├── vpn-setup/             # composite action (chama vpnctl conectar)
    └── vpn-teardown/          # composite action (chama vpnctl desconectar)
```

## Docker

O `vpnctl` também é publicado como imagem Docker no Docker Hub, para uso em
qualquer CI (GitLab CI, Jenkins, CircleCI) ou localmente.

### Pull

```yaml
docker pull docker.io/vcybertech/vpnctl:1
```

### Uso

A imagem precisa de rede do host e permissão para criar a interface `tun`:

```yaml
docker run --rm \
    --network=host \
    --cap-add=NET_ADMIN \
    -e RUNNER_TEMP=/work \
    -v /caminho/config.ovpn:/input/config.ovpn:ro \
    -v /tmp/vpn-work:/work \
    docker.io/vcybertech/vpnctl:1 \
    conectar --config /input/config.ovpn \
             --healthcheck-host internal.dns.example --healthcheck-port 53
```

**Flags obrigatórias:**

- `--network=host` — sem isso, o túnel fica isolado dentro do container e o host não vê a interface `tun`
- `--cap-add=NET_ADMIN` — sem isso, o `openvpn` não consegue criar a interface
- `-e RUNNER_TEMP=/work` + `-v /tmp/vpn-work:/work` — para que o `desconectar` encontre o PID file

### Tags disponíveis

| Tag | Significado |
|---|---|
| `1.0.1` | versão exata |
| `1` | última versão estável da major 1 |
| `latest` | última versão estável |

### Encerrar o túnel

```bash
docker run --rm --network=host --cap-add=NET_ADMIN \
    -e RUNNER_TEMP=/work \
    -v /tmp/vpn-work:/work \
    docker.io/vcybertech/vpnctl:1 \
    desconectar
```

## Desenvolvimento

Requisitos: Rust 1.85+ (edição 2024), alvo x86_64-unknown-linux-musl instalado.

```yaml
cd tools
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Publicação de releases

Cada ação é versionada independentemente com tags no formato
<nome-da-ação>/vX.Y.Z. A tag flutuante <nome-da-ação>/v1 aponta para
a release estável mais recente

## Licença

MIT
