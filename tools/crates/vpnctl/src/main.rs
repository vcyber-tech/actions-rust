//! vpnctl — Gerencia ciclo de vida da VPN nas Runners para CICD
//!
//! Este módulo implementa a superfície da CLI e a orquestração dos
//! subcomandos `conectar` e `desconectar`. No momento, apenas OpenVPN
//! é suportado.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use toolcore::{emite_anotacao, escreve_output, mascara_env_logs};

#[derive(Parser)]
#[command(
    name = "vpnctl",
    version,
    about = "Gerencia ciclo de vida da VPN nas Runners para CICD"
)]
struct Cli {
    #[command(subcommand)]
    comando: Comando,
}

#[derive(Subcommand)]
enum Comando {
    /// Estabelece o túnel VPN e valida a conectividade.
    Conectar(ArgsConectar),
    /// Derruba o túnel VPN (idempotente).
    Desconectar,
}

#[derive(Parser)]
struct ArgsConectar {
    /// Provedor de VPN a utilizar.
    #[arg(long, value_enum, default_value_t = Provedor::Openvpn)]
    provider: Provedor,

    /// Caminho para o arquivo de configuração (.ovpn ou .conf).
    #[arg(long, env = "VPN_CONFIG_PATH")]
    config: PathBuf,

    /// Nome de usuário (opcional — alguns provedores usam apenas certificado).
    #[arg(long, env = "VPN_USERNAME")]
    username: Option<String>,

    /// Senha (lida preferencialmente do ambiente, nunca de argumento em produção).
    #[arg(long, env = "VPN_PASSWORD")]
    password: Option<String>,

    /// Host interno para healthcheck após subir o túnel.
    #[arg(long)]
    healthcheck_host: Option<String>,

    /// Porta do healthcheck.
    #[arg(long, default_value_t = 443)]
    healthcheck_port: u16,

    /// Timeout total, em segundos, para considerar a conexão bem-sucedida.
    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,

    /// Nome esperado da interface do túnel (auto-detectado se omitido).
    #[arg(long)]
    interface: Option<String>,

    /// Executa toda a validação sem subir o túnel de verdade.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

/// Provedores de VPN suportados.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Provedor {
    Openvpn,
}

impl Provedor {
    /// Prefixo comum das interfaces deste provedor.
    fn prefixo_interface(&self) -> &'static str {
        match self {
            Provedor::Openvpn => "tun",
        }
    }
}

fn main() {
    if let Err(erro) = executa() {
        emite_anotacao("error", &format!("{erro:#}"));
        std::process::exit(1);
    }
}

fn executa() -> Result<()> {
    match Cli::parse().comando {
        Comando::Conectar(args) => conectar(args),
        Comando::Desconectar => desconectar(),
    }
}

// ---------------- Subcomando: conectar ----------------

fn conectar(args: ArgsConectar) -> Result<()> {
    let workdir = diretorio_trabalho()?;
    conectar_com(args, &AmbienteReal, &workdir)
}

/// Núcleo testável do subcomando `conectar`.
fn conectar_com(args: ArgsConectar, ambiente: &dyn Ambiente, workdir: &Path) -> Result<()> {
    valida_precondicoes(&args)?;
    mascara_credenciais(&args);

    let config_segura = escreve_config_segura(&args, workdir)?;
    let auth_file = prepara_auth_file(&args, workdir)?;

    if args.dry_run {
        println!("dry-run: validação concluída sem subir o túnel");
        println!("  config segura: {}", config_segura.display());
        if let Some(a) = &auth_file {
            println!("  auth file:     {}", a.display());
        }
        println!("  provider:      {:?}", args.provider);
        return Ok(());
    }

    executa_cliente(
        ambiente,
        &args,
        &config_segura,
        auth_file.as_deref(),
        workdir,
    )?;

    let timeout = Duration::from_secs(args.timeout_secs);
    let interface = descobre_interface(ambiente, &args, timeout)?;
    let ip = descobre_ip(ambiente, &interface, timeout)?;
    println!("túnel ativo em {interface} ({ip})");

    // Publica nos outputs do GitHub Actions ($GITHUB_OUTPUT). Fora do
    // runner, a função cai no stdout em formato `key=value`.
    escreve_output("interface", &interface)?;
    escreve_output("tunnel-ip", &ip)?;

    if let Some(host) = &args.healthcheck_host {
        verifica_healthcheck(ambiente, host, args.healthcheck_port, timeout)?;
    } else {
        emite_anotacao(
            "warning",
            "healthcheck não configurado — conectividade pós-túnel não foi validada",
        );
    }

    Ok(())
}

// ---------------- Subcomando: desconectar ----------------

fn desconectar() -> Result<()> {
    let workdir = diretorio_trabalho()?;
    desconectar_com(&AmbienteReal, &workdir)
}

/// Núcleo testável do subcomando `desconectar`. Idempotente: rodar duas
/// vezes não é erro, e rodar sem nunca ter conectado também não.
///
/// Etapas:
///   1. Se houver PID file, envia SIGTERM e aguarda o processo sair.
///   2. Remove os arquivos de trabalho (pid, config, auth).
fn desconectar_com(ambiente: &dyn Ambiente, workdir: &Path) -> Result<()> {
    let pid_file = workdir.join("openvpn.pid");

    match le_pid_do_arquivo(&pid_file)? {
        Some(pid) => {
            println!("encerrando openvpn (pid {pid})");
            ambiente.envia_sinal_termino(pid)?;
            aguarda_processo_morrer(ambiente, pid, Duration::from_secs(10))?;
            println!("openvpn encerrado");
        }
        None => {
            println!("nenhum openvpn em execução");
        }
    }

    limpa_arquivos_de_trabalho(workdir);
    Ok(())
}

/// Lê o PID do arquivo, se ele existir. Devolve `Ok(None)` quando o
/// arquivo não existe — distinto de erro de leitura ou formato inválido.
fn le_pid_do_arquivo(pid_file: &Path) -> Result<Option<u32>> {
    if !pid_file.exists() {
        return Ok(None);
    }
    let conteudo = std::fs::read_to_string(pid_file)
        .with_context(|| format!("falha ao ler {}", pid_file.display()))?;
    let pid = conteudo
        .trim()
        .parse::<u32>()
        .with_context(|| format!("conteúdo inválido em {}", pid_file.display()))?;
    Ok(Some(pid))
}

/// Aguarda o processo deixar de existir, com polling de 200ms até o
/// timeout. Se ainda estiver vivo ao final, devolve erro.
fn aguarda_processo_morrer(ambiente: &dyn Ambiente, pid: u32, timeout: Duration) -> Result<()> {
    let inicio = Instant::now();
    let intervalo = Duration::from_millis(200);

    while inicio.elapsed() < timeout {
        if !ambiente.processo_vivo(pid)? {
            return Ok(());
        }
        sleep(intervalo);
    }

    bail!(
        "openvpn (pid {pid}) não encerrou em {}s após SIGTERM",
        timeout.as_secs()
    )
}

/// Remove arquivos de trabalho. Erros são ignorados de propósito — se o
/// arquivo não existe, não é problema.
fn limpa_arquivos_de_trabalho(workdir: &Path) {
    for nome in ["openvpn.pid", "vpn.conf", "auth.txt"] {
        let _ = std::fs::remove_file(workdir.join(nome));
    }
}

// ---------------- Abstração de ambiente ----------------

/// Operações que dependem do sistema operacional, isoladas atrás de um
/// trait para permitir testes determinísticos.
trait Ambiente {
    /// Lista nomes de interfaces de rede.
    fn lista_interfaces(&self) -> Result<Vec<String>>;

    /// Executa `ip -4 -o addr show dev <iface>` e extrai o IPv4.
    /// Devolve `Ok(None)` se a interface ainda não tem IPv4 configurado.
    fn le_ipv4_da_interface(&self, interface: &str) -> Result<Option<String>>;

    /// Inicia o OpenVPN em modo daemon.
    fn executa_openvpn(&self, config: &Path, auth: Option<&Path>, pid_file: &Path) -> Result<()>;

    /// Tenta uma conexão TCP única contra `host:porta`.
    fn tenta_conexao_tcp(&self, host: &str, porta: u16, timeout: Duration) -> Result<()>;

    /// Envia SIGTERM ao PID indicado. `No such process` é tratado como
    /// sucesso para manter a operação idempotente.
    fn envia_sinal_termino(&self, pid: u32) -> Result<()>;

    /// Devolve `true` se o processo existe e temos permissão para sinalizá-lo.
    fn processo_vivo(&self, pid: u32) -> Result<bool>;
}

/// Implementação real, executando os comandos do sistema.
struct AmbienteReal;

impl Ambiente for AmbienteReal {
    fn lista_interfaces(&self) -> Result<Vec<String>> {
        let saida = Command::new("ip")
            .args(["-o", "link", "show"])
            .stdin(Stdio::null())
            .output()
            .context("falha ao executar `ip link show` — iproute2 instalado?")?;

        if !saida.status.success() {
            bail!(
                "`ip link show` retornou {}: {}",
                saida.status,
                String::from_utf8_lossy(&saida.stderr).trim()
            );
        }

        let stdout = String::from_utf8_lossy(&saida.stdout);
        Ok(stdout
            .lines()
            .filter_map(|linha| {
                // Formato típico: "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> ..."
                linha
                    .split_whitespace()
                    .nth(1)
                    .map(|s| s.trim_end_matches(':').to_string())
            })
            .collect())
    }

    fn le_ipv4_da_interface(&self, interface: &str) -> Result<Option<String>> {
        let saida = Command::new("ip")
            .args(["-4", "-o", "addr", "show", "dev", interface])
            .stdin(Stdio::null())
            .output()
            .context("falha ao executar `ip -4 -o addr show`")?;

        if !saida.status.success() {
            // Interface ainda não existe ou sem IP — None para o polling seguir.
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&saida.stdout);
        Ok(extrai_ipv4(&stdout))
    }

    fn executa_openvpn(&self, config: &Path, auth: Option<&Path>, pid_file: &Path) -> Result<()> {
        let mut cmd = Command::new("openvpn");
        cmd.arg("--config").arg(config);
        cmd.arg("--daemon");
        cmd.arg("--writepid").arg(pid_file);

        if let Some(auth_path) = auth {
            cmd.arg("--auth-user-pass").arg(auth_path);
        }

        // `stdin(Stdio::null())` evita que o cliente trave esperando input
        // interativo — problema que só apareceria em CI, sem TTY.
        let saida = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| "falha ao executar openvpn — binário instalado e acessível?")?;

        if !saida.status.success() {
            let stderr = String::from_utf8_lossy(&saida.stderr);
            let stdout = String::from_utf8_lossy(&saida.stdout);
            bail!(
                "openvpn retornou {} — stderr: {} | stdout: {}",
                saida.status,
                stderr.trim(),
                stdout.trim()
            );
        }

        Ok(())
    }

    fn tenta_conexao_tcp(&self, host: &str, porta: u16, timeout: Duration) -> Result<()> {
        let endereco = format!("{host}:{porta}");
        let enderecos: Vec<_> = endereco
            .to_socket_addrs()
            .with_context(|| format!("não foi possível resolver {endereco}"))?
            .collect();

        if enderecos.is_empty() {
            bail!("nenhum endereço resolvido para {endereco}");
        }

        let mut ultimo_erro: Option<std::io::Error> = None;
        for addr in &enderecos {
            match TcpStream::connect_timeout(addr, timeout) {
                Ok(_) => return Ok(()),
                Err(e) => ultimo_erro = Some(e),
            }
        }

        let msg = ultimo_erro
            .map(|e| e.to_string())
            .unwrap_or_else(|| "erro desconhecido".to_string());
        bail!("connect TCP para {endereco} falhou em todos os endereços: {msg}")
    }

    fn envia_sinal_termino(&self, pid: u32) -> Result<()> {
        let saida = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .output()
            .context("falha ao executar `kill -TERM`")?;

        if !saida.status.success() {
            let stderr = String::from_utf8_lossy(&saida.stderr);
            // `kill` devolve "No such process" quando o PID não existe mais.
            // Tratamos como sucesso para manter `desconectar` idempotente.
            if stderr.contains("No such process") {
                return Ok(());
            }
            bail!("kill -TERM {pid} falhou: {}", stderr.trim());
        }
        Ok(())
    }

    fn processo_vivo(&self, pid: u32) -> Result<bool> {
        // `kill -0 <pid>` não envia sinal — só testa existência e permissão.
        let saida = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .output()
            .context("falha ao executar `kill -0`")?;
        Ok(saida.status.success())
    }
}

// ---------------- Healthcheck ----------------

/// Verifica conectividade TCP real contra `host:porta`, com retentativas
/// até o timeout total. Cada tentativa individual tem teto de 5s.
fn verifica_healthcheck(
    ambiente: &dyn Ambiente,
    host: &str,
    porta: u16,
    timeout: Duration,
) -> Result<()> {
    let inicio = Instant::now();
    let mut tentativa: u32 = 0;

    loop {
        tentativa += 1;

        match ambiente.tenta_conexao_tcp(host, porta, Duration::from_secs(5)) {
            Ok(()) => {
                println!("healthcheck OK em {host}:{porta} (tentativa {tentativa})");
                return Ok(());
            }
            Err(erro) => {
                if inicio.elapsed() >= timeout {
                    bail!(
                        "healthcheck falhou em {host}:{porta} após {tentativa} tentativa(s) em {}s: {erro:#}",
                        timeout.as_secs()
                    );
                }
                let espera = calcula_backoff(tentativa - 1);
                println!(
                    "healthcheck tentativa {tentativa} falhou ({erro:#}); retentando em {:.2}s",
                    espera.as_secs_f64()
                );
                sleep(espera);
            }
        }
    }
}

/// Backoff exponencial com jitter, em milissegundos.
fn calcula_backoff(tentativa: u32) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 8_000;
    const JITTER_MS: u64 = 200;

    let expoente = tentativa.min(4);
    let base = BASE_MS.saturating_mul(1u64 << expoente).min(CAP_MS);
    let jitter = fastrand::u64(0..JITTER_MS);
    Duration::from_millis(base + jitter)
}

// ---------------- Descoberta de interface ----------------

fn descobre_interface(
    ambiente: &dyn Ambiente,
    args: &ArgsConectar,
    timeout: Duration,
) -> Result<String> {
    let inicio = Instant::now();
    let intervalo = Duration::from_millis(500);

    loop {
        let interfaces = ambiente
            .lista_interfaces()
            .context("falha ao listar interfaces de rede")?;

        let encontrada = match &args.interface {
            Some(nome) => interfaces.iter().any(|i| i == nome).then(|| nome.clone()),
            None => {
                let prefixo = args.provider.prefixo_interface();
                interfaces.into_iter().find(|i| i.starts_with(prefixo))
            }
        };

        if let Some(iface) = encontrada {
            return Ok(iface);
        }

        if inicio.elapsed() >= timeout {
            let alvo = match &args.interface {
                Some(nome) => format!("interface '{nome}'"),
                None => format!(
                    "interface com prefixo '{}*'",
                    args.provider.prefixo_interface()
                ),
            };
            bail!(
                "timeout de {}s aguardando {alvo} aparecer",
                timeout.as_secs()
            );
        }

        sleep(intervalo);
    }
}

/// Extrai o primeiro IPv4 (sem o prefixo CIDR) do output de
/// `ip -4 -o addr show`. Função pura, testada diretamente.
fn extrai_ipv4(stdout: &str) -> Option<String> {
    for linha in stdout.lines() {
        let campos: Vec<&str> = linha.split_whitespace().collect();
        if let Some(pos) = campos.iter().position(|&c| c == "inet") {
            if let Some(cidr) = campos.get(pos + 1) {
                return cidr.split('/').next().map(String::from);
            }
        }
    }
    None
}

// ---------------- Descoberta de IPv4 ----------------

fn descobre_ip(ambiente: &dyn Ambiente, interface: &str, timeout: Duration) -> Result<String> {
    let inicio = Instant::now();
    let intervalo = Duration::from_millis(500);

    loop {
        if let Some(ip) = ambiente.le_ipv4_da_interface(interface)? {
            return Ok(ip);
        }

        if inicio.elapsed() >= timeout {
            bail!(
                "timeout de {}s aguardando IPv4 na interface '{interface}'",
                timeout.as_secs()
            );
        }

        sleep(intervalo);
    }
}

// ---------------- Execução do cliente ----------------

fn executa_cliente(
    ambiente: &dyn Ambiente,
    args: &ArgsConectar,
    config: &Path,
    auth: Option<&Path>,
    workdir: &Path,
) -> Result<()> {
    match args.provider {
        Provedor::Openvpn => {
            let pid_file = workdir.join("openvpn.pid");
            ambiente.executa_openvpn(config, auth, &pid_file)?;
            println!(
                "openvpn iniciado em modo daemon (pid file: {})",
                pid_file.display()
            );
            Ok(())
        }
    }
}

// ---------------- Pré-condições ----------------

fn valida_precondicoes(args: &ArgsConectar) -> Result<()> {
    if !args.config.exists() {
        bail!(
            "arquivo de configuração não encontrado: {}",
            args.config.display()
        );
    }
    if !args.config.is_file() {
        bail!(
            "caminho de configuração não é um arquivo regular: {}",
            args.config.display()
        );
    }
    // Senha só faz sentido com usuário, e vice-versa. Tratamos string
    // vazia como ausente — é o que o GitHub Actions passa quando o input
    // não foi preenchido.
    let usuario = args.username.as_deref().filter(|s| !s.is_empty());
    let senha = args.password.as_deref().filter(|s| !s.is_empty());
    match (usuario, senha) {
        (Some(_), None) => bail!("--username foi informado mas --password está ausente"),
        (None, Some(_)) => bail!("--password foi informado mas --username está ausente"),
        _ => {}
    }
    Ok(())
}

fn mascara_credenciais(args: &ArgsConectar) {
    if let Some(senha) = &args.password {
        mascara_env_logs(senha);
    }
}

// ---------------- Escrita segura ----------------

/// Escreve o conteúdo da config no workdir, com permissões restritas ao
/// dono (0600), e devolve o caminho resultante.
fn escreve_config_segura(args: &ArgsConectar, workdir: &Path) -> Result<PathBuf> {
    let conteudo = std::fs::read_to_string(&args.config)
        .with_context(|| format!("falha ao ler config: {}", args.config.display()))?;

    let destino = workdir.join("vpn.conf");

    std::fs::write(&destino, conteudo)
        .with_context(|| format!("falha ao escrever config segura em {}", destino.display()))?;
    restringe_permissoes(&destino, 0o600)?;

    Ok(destino)
}

/// Prepara o arquivo de autenticação (`usuário\nsenha\n`) se ambos foram
/// informados. Retorna `None` quando o provedor usa apenas certificado.
fn prepara_auth_file(args: &ArgsConectar, workdir: &Path) -> Result<Option<PathBuf>> {
    // String vazia conta como ausente — o GitHub Actions passa `''`
    // quando o input não é preenchido.
    let Some(usuario) = args.username.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(senha) = args.password.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    let destino = workdir.join("auth.txt");
    let conteudo = format!("{usuario}\n{senha}\n");

    std::fs::write(&destino, conteudo)
        .with_context(|| format!("falha ao escrever auth file em {}", destino.display()))?;
    restringe_permissoes(&destino, 0o600)?;

    Ok(Some(destino))
}

/// Devolve (criando se necessário) o diretório de trabalho compartilhado
/// entre `conectar` e `desconectar` no mesmo job.
///
/// Usa `$RUNNER_TEMP/vpnctl` quando definido (GitHub Actions) ou
/// `/tmp/vpnctl` em desenvolvimento local. É intencionalmente fixo — se
/// fosse por PID, o `desconectar` (processo diferente) não encontraria o
/// PID file escrito pelo `conectar`.
fn diretorio_trabalho() -> Result<PathBuf> {
    let base = std::env::var_os("RUNNER_TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("vpnctl");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("falha ao criar diretório de trabalho {}", dir.display()))?;
    Ok(dir)
}

#[cfg(unix)]
fn restringe_permissoes(caminho: &Path, modo: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(caminho, std::fs::Permissions::from_mode(modo))
        .with_context(|| format!("falha ao restringir permissões de {}", caminho.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn restringe_permissoes(_caminho: &Path, _modo: u32) -> Result<()> {
    // Este binário é destinado a Runners Linux; aceitamos no-op nas
    // outras plataformas para permitir compilação cruzada.
    Ok(())
}

// ---------------- Testes ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrai_ipv4_de_saida_tipica() {
        let saida = "5: tun0    inet 10.8.0.2/24 brd 10.8.0.255 scope global tun0\\       valid_lft forever preferred_lft forever";
        assert_eq!(extrai_ipv4(saida), Some("10.8.0.2".to_string()));
    }

    #[test]
    fn extrai_ipv4_de_ip_sem_prefixo_cidr() {
        let saida = "5: tun0    inet 10.8.0.2 scope global tun0";
        assert_eq!(extrai_ipv4(saida), Some("10.8.0.2".to_string()));
    }

    #[test]
    fn extrai_ipv4_retorna_none_para_saida_vazia() {
        assert_eq!(extrai_ipv4(""), None);
    }

    #[test]
    fn extrai_ipv4_retorna_none_quando_nao_ha_inet() {
        let saida = "5: tun0    <POINTOPOINT,MULTICAST,NOARP,UP,LOWER_UP> mtu 1500 state UNKNOWN";
        assert_eq!(extrai_ipv4(saida), None);
    }

    #[test]
    fn extrai_ipv4_ignora_linha_em_branco_inicial() {
        let saida = "\n5: tun0    inet 10.8.0.2/24 scope global tun0";
        assert_eq!(extrai_ipv4(saida), Some("10.8.0.2".to_string()));
    }

    #[test]
    fn calcula_backoff_cresce_com_tentativas() {
        let menor = |n: u32| {
            (0..30)
                .map(|_| calcula_backoff(n).as_millis())
                .min()
                .unwrap()
        };
        assert!(menor(0) < menor(1), "tentativa 0 deve ser menor que 1");
        assert!(menor(1) < menor(2), "tentativa 1 deve ser menor que 2");
        assert!(menor(2) < menor(3), "tentativa 2 deve ser menor que 3");
    }

    #[test]
    fn calcula_backoff_tem_teto() {
        let maior = (0..50)
            .map(|_| calcula_backoff(10).as_millis())
            .max()
            .unwrap();
        assert!(
            maior <= 8_200,
            "backoff não deve passar de 8s + 200ms de jitter, veio {maior}ms"
        );
    }

    #[test]
    fn calcula_backoff_respeita_limite_de_jitter() {
        for tentativa in 0..5u32 {
            let base_esperada = 500u128 * 2u128.pow(tentativa.min(4));
            let base = base_esperada.min(8_000);
            for _ in 0..30 {
                let ms = calcula_backoff(tentativa).as_millis();
                assert!(
                    ms >= base && ms < base + 200,
                    "tentativa={tentativa} ms={ms} base={base}"
                );
            }
        }
    }

    #[test]
    fn ambiente_real_conecta_a_listener_ativo() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let porta = listener.local_addr().unwrap().port();

        let resultado = AmbienteReal.tenta_conexao_tcp("127.0.0.1", porta, Duration::from_secs(2));
        assert!(resultado.is_ok(), "esperava sucesso, veio {resultado:?}");
    }

    #[test]
    fn ambiente_real_falha_em_porta_fechada() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let porta = listener.local_addr().unwrap().port();
        drop(listener);

        let resultado =
            AmbienteReal.tenta_conexao_tcp("127.0.0.1", porta, Duration::from_millis(500));
        assert!(resultado.is_err(), "esperava erro, veio {resultado:?}");
    }

    #[test]
    fn le_pid_do_arquivo_retorna_none_quando_ausente() {
        let path = std::env::temp_dir().join("vpnctl-nao-existe.pid");
        let _ = std::fs::remove_file(&path);
        assert_eq!(le_pid_do_arquivo(&path).unwrap(), None);
    }
}

// ---------------- Fake de ambiente ----------------

#[cfg(test)]
mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    /// Implementação controlável de [`Ambiente`], usada apenas em testes.
    pub struct AmbienteFake {
        pub interfaces: RefCell<Vec<String>>,
        pub ips: RefCell<HashMap<String, String>>,
        pub openvpn_erro: RefCell<Option<String>>,
        pub tcp_erro: RefCell<Option<String>>,
        pub openvpn_chamado: RefCell<usize>,
        pub tcp_chamadas: RefCell<usize>,
        pub sinais_enviados: RefCell<Vec<u32>>,
        pub processos_vivos: RefCell<HashSet<u32>>,
    }

    impl AmbienteFake {
        pub fn novo() -> Self {
            Self {
                interfaces: RefCell::new(Vec::new()),
                ips: RefCell::new(HashMap::new()),
                openvpn_erro: RefCell::new(None),
                tcp_erro: RefCell::new(None),
                openvpn_chamado: RefCell::new(0),
                tcp_chamadas: RefCell::new(0),
                sinais_enviados: RefCell::new(Vec::new()),
                processos_vivos: RefCell::new(HashSet::new()),
            }
        }

        pub fn adiciona_interface(&self, nome: &str) {
            self.interfaces.borrow_mut().push(nome.into());
        }

        pub fn adiciona_ip(&self, iface: &str, ip: &str) {
            self.ips.borrow_mut().insert(iface.into(), ip.into());
        }
    }

    impl Ambiente for AmbienteFake {
        fn lista_interfaces(&self) -> Result<Vec<String>> {
            Ok(self.interfaces.borrow().clone())
        }

        fn le_ipv4_da_interface(&self, interface: &str) -> Result<Option<String>> {
            Ok(self.ips.borrow().get(interface).cloned())
        }

        fn executa_openvpn(
            &self,
            _config: &Path,
            _auth: Option<&Path>,
            _pid_file: &Path,
        ) -> Result<()> {
            *self.openvpn_chamado.borrow_mut() += 1;
            match &*self.openvpn_erro.borrow() {
                Some(msg) => bail!("{msg}"),
                None => Ok(()),
            }
        }

        fn tenta_conexao_tcp(&self, _host: &str, _porta: u16, _timeout: Duration) -> Result<()> {
            *self.tcp_chamadas.borrow_mut() += 1;
            match &*self.tcp_erro.borrow() {
                Some(msg) => bail!("{msg}"),
                None => Ok(()),
            }
        }

        fn envia_sinal_termino(&self, pid: u32) -> Result<()> {
            self.sinais_enviados.borrow_mut().push(pid);
            // Simula morte graciosa imediata: remove do conjunto de vivos.
            self.processos_vivos.borrow_mut().remove(&pid);
            Ok(())
        }

        fn processo_vivo(&self, pid: u32) -> Result<bool> {
            Ok(self.processos_vivos.borrow().contains(&pid))
        }
    }
}

// ---------------- Testes end-to-end ----------------

#[cfg(test)]
mod tests_e2e {
    use super::fake::AmbienteFake;
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // Serializa testes que leem/escrevem variáveis de ambiente, em
    // especial GITHUB_OUTPUT. `std::env::set_var` é `unsafe` desde
    // Rust 1.86 (edição 2024) porque mutação de env não é thread-safe.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cria_workdir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("vpnctl-e2e-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cria_config() -> PathBuf {
        let dir = cria_workdir();
        let path = dir.join("test.ovpn");
        std::fs::write(&path, "client\ndev tun\n").unwrap();
        path
    }

    fn args_padrao(config: PathBuf) -> ArgsConectar {
        ArgsConectar {
            provider: Provedor::Openvpn,
            config,
            username: None,
            password: None,
            healthcheck_host: None,
            healthcheck_port: 443,
            timeout_secs: 1,
            interface: None,
            dry_run: false,
        }
    }

    // --- conectar ---

    #[test]
    fn sucesso_completo_com_healthcheck() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        fake.adiciona_ip("tun0", "10.8.0.2");
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.healthcheck_host = Some("servidor.interno".into());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert_eq!(*fake.openvpn_chamado.borrow(), 1);
        assert_eq!(*fake.tcp_chamadas.borrow(), 1);
    }

    #[test]
    fn sucesso_sem_healthcheck_emite_warning() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        fake.adiciona_ip("tun0", "10.8.0.2");
        let workdir = cria_workdir();

        let args = args_padrao(cria_config());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert_eq!(*fake.tcp_chamadas.borrow(), 0, "não deve tentar TCP");
    }

    #[test]
    fn falha_do_openvpn_interrompe_fluxo() {
        let fake = AmbienteFake::novo();
        *fake.openvpn_erro.borrow_mut() = Some("config inválida".into());
        let workdir = cria_workdir();

        let args = args_padrao(cria_config());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_err());
        let msg = format!("{:#}", resultado.unwrap_err());
        assert!(msg.contains("config inválida"), "{msg}");
        assert_eq!(*fake.tcp_chamadas.borrow(), 0, "não deve chegar no TCP");
    }

    #[test]
    fn timeout_esperando_interface() {
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        let args = args_padrao(cria_config());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_err());
        let msg = format!("{:#}", resultado.unwrap_err());
        assert!(msg.contains("timeout"), "{msg}");
    }

    #[test]
    fn timeout_esperando_ip() {
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        let workdir = cria_workdir();

        let args = args_padrao(cria_config());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_err());
        let msg = format!("{:#}", resultado.unwrap_err());
        assert!(msg.contains("IPv4"), "{msg}");
    }

    #[test]
    fn falha_do_healthcheck_apos_tunel_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        fake.adiciona_ip("tun0", "10.8.0.2");
        *fake.tcp_erro.borrow_mut() = Some("connection refused".into());
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.healthcheck_host = Some("servidor.interno".into());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_err());
        let msg = format!("{:#}", resultado.unwrap_err());
        assert!(msg.contains("healthcheck"), "{msg}");
        assert!(*fake.tcp_chamadas.borrow() >= 1, "deve ter tentado TCP");
    }

    #[test]
    fn dry_run_nao_chama_ambiente() {
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.dry_run = true;

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert_eq!(*fake.openvpn_chamado.borrow(), 0);
        assert_eq!(*fake.tcp_chamadas.borrow(), 0);
    }

    #[test]
    fn interface_explicita_e_respeitada() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("outra0");
        fake.adiciona_interface("tun5");
        fake.adiciona_ip("tun5", "10.9.0.1");
        fake.adiciona_ip("outra0", "192.168.1.1");
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.interface = Some("tun5".into());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
    }

    // --- desconectar ---

    #[test]
    fn auth_file_nao_eh_criado_com_credenciais_vazias() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Regressão: GitHub Actions passa `''` quando o input não é
        // preenchido. Sem essa proteção, criávamos auth.txt com "\n\n".
        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        fake.adiciona_ip("tun0", "10.8.0.2");
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.username = Some(String::new());
        args.password = Some(String::new());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert!(
            !workdir.join("auth.txt").exists(),
            "auth.txt não deve existir quando credenciais são vazias"
        );
    }

    #[test]
    fn usuario_vazio_com_senha_preenchida_eh_rejeitado() {
        // Cobertura do valida_precondicoes com o filtro de vazio.
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        let mut args = args_padrao(cria_config());
        args.username = Some(String::new());
        args.password = Some("senha-real".into());

        let resultado = conectar_com(args, &fake, &workdir);
        assert!(resultado.is_err());
        let msg = format!("{:#}", resultado.unwrap_err());
        assert!(msg.contains("--password"), "{msg}");
    }

    #[test]
    fn conectar_com_publica_outputs_em_github_output() {
        let _guard = ENV_LOCK.lock().unwrap();

        let outdir = cria_workdir();
        let gh_output = outdir.join("github_output.txt");
        std::fs::write(&gh_output, "").unwrap();

        // SAFETY: mutação de env protegida por ENV_LOCK. Nenhum outro
        // teste desta suíte lê/escreve GITHUB_OUTPUT fora do lock.
        unsafe { std::env::set_var("GITHUB_OUTPUT", &gh_output) };

        let fake = AmbienteFake::novo();
        fake.adiciona_interface("tun0");
        fake.adiciona_ip("tun0", "10.8.0.2");
        let args = args_padrao(cria_config());

        let resultado = conectar_com(args, &fake, &cria_workdir());

        unsafe { std::env::remove_var("GITHUB_OUTPUT") };

        assert!(resultado.is_ok(), "{resultado:?}");

        let conteudo = std::fs::read_to_string(&gh_output).unwrap();
        assert!(
            conteudo.contains("interface=tun0"),
            "output 'interface' não publicado; conteúdo foi:\n{conteudo}"
        );
        assert!(
            conteudo.contains("tunnel-ip=10.8.0.2"),
            "output 'tunnel-ip' não publicado; conteúdo foi:\n{conteudo}"
        );
    }

    #[test]
    fn conectar_com_em_falha_nao_publica_outputs() {
        let _guard = ENV_LOCK.lock().unwrap();

        let outdir = cria_workdir();
        let gh_output = outdir.join("github_output.txt");
        std::fs::write(&gh_output, "").unwrap();

        unsafe { std::env::set_var("GITHUB_OUTPUT", &gh_output) };

        // Fake sem interfaces: descobre_interface esgota o timeout.
        let fake = AmbienteFake::novo();
        let mut args = args_padrao(cria_config());
        args.timeout_secs = 1;

        let resultado = conectar_com(args, &fake, &cria_workdir());

        unsafe { std::env::remove_var("GITHUB_OUTPUT") };

        assert!(
            resultado.is_err(),
            "esperava falha por timeout de interface"
        );
        let conteudo = std::fs::read_to_string(&gh_output).unwrap();
        assert!(
            !conteudo.contains("interface="),
            "outputs não deveriam ter sido publicados em falha; conteúdo foi:\n{conteudo}"
        );
    }

    #[test]
    fn desconectar_sem_pid_file_eh_idempotente() {
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        // primeira chamada — nada para desconectar
        let r1 = desconectar_com(&fake, &workdir);
        assert!(r1.is_ok(), "{r1:?}");
        assert!(fake.sinais_enviados.borrow().is_empty());

        // segunda chamada — mesmo resultado
        let r2 = desconectar_com(&fake, &workdir);
        assert!(r2.is_ok(), "{r2:?}");
        assert!(fake.sinais_enviados.borrow().is_empty());
    }

    #[test]
    fn desconectar_encerra_processo_do_pid_file() {
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        // Simula PID 4242 vivo, e escreve o pid file correspondente
        fake.processos_vivos.borrow_mut().insert(4242);
        std::fs::write(workdir.join("openvpn.pid"), "4242\n").unwrap();

        let resultado = desconectar_com(&fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert_eq!(*fake.sinais_enviados.borrow(), vec![4242u32]);
        assert!(
            !fake.processo_vivo(4242).unwrap(),
            "processo deve estar morto"
        );
        assert!(!workdir.join("openvpn.pid").exists(), "pid file removido");
    }

    #[test]
    fn desconectar_limpa_arquivos_de_trabalho() {
        let fake = AmbienteFake::novo();
        let workdir = cria_workdir();

        // Cria arquivos residuais como o `conectar` faria
        std::fs::write(workdir.join("vpn.conf"), "config").unwrap();
        std::fs::write(workdir.join("auth.txt"), "user\npass\n").unwrap();
        // Sem pid file — simula um conectar que falhou antes de iniciar openvpn

        let resultado = desconectar_com(&fake, &workdir);
        assert!(resultado.is_ok(), "{resultado:?}");
        assert!(!workdir.join("vpn.conf").exists(), "vpn.conf removido");
        assert!(!workdir.join("auth.txt").exists(), "auth.txt removido");
    }
}
