# A imagem contém o binário vpnctl e as dependências de runtime do
# OpenVPN. O binário em si é estático (musl), mas o cliente `openvpn` e
# o utilitário `ip` vêm do Alpine.
#
# O binário é copiado para dentro do contexto de build pelo workflow de
# release, que baixa o artefato `vpnctl` do job `build`.
FROM alpine:3.24.2

# Metadados OCI — lidos por ferramentas de supply chain (Renovate,
# scanners) para ligar a imagem ao repositório-fonte.
LABEL org.opencontainers.image.source="https://github.com/vcyber-tech/actions-rust"

# Dependências de runtime:
#   openvpn        — estabelece o túnel
#   iproute2       — fornece `ip`, usado para descobrir interface e IP
#   ca-certificates — validação TLS do servidor OpenVPN
#   bash           — algumas configs .ovpn invocam scripts auxiliares
RUN apk add --no-cache \
        openvpn \
        iproute2 \
        ca-certificates \
        bash \
    && rm -rf /var/cache/apk/*

# Copia o binário pré-compilado (musl, x86_64) para o PATH padrão.
COPY vpnctl /usr/local/bin/vpnctl

# O openvpn precisa de CAP_NET_ADMIN para criar a interface tun. O
# container deve ser executado com:
#   docker run --network=host --cap-add=NET_ADMIN ...
#
# Nota: o scanner do Docker Hub pode reportar CVE-2026-11604 para o pacote
# `openvpn` do Alpine. Esse CVE afeta especificamente o driver `ovpn-dco-win`
# (Windows). O componente não existe na plataforma Linux, portanto a imagem
# não é afetada. Ver: https://security.alpinelinux.org/vuln/CVE-2026-11604
ENTRYPOINT ["/usr/local/bin/vpnctl"]
CMD ["--help"]
