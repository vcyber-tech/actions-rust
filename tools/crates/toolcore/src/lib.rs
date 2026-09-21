//! Funções compartilhadas entre ferramentas actions-rust.
//!
//! Funções utilitárias usadas por todos os binários do workspace:
//! interação com o protocolo do GitHub Actions (outputs, anotações,
//! mascaramento de segredos) e leitura de variáveis de ambiente.
//!
//! Todas as funções degradam graciosamente fora do runner, o que
//! permite desenvolver e testar localmente sem simular o GitHub Actions.

use std::env;
use std::io::Write;

use anyhow::{Result, anyhow};

/// Retorna `true` se o código está executando dentro de uma runner do
/// GitHub Actions, detectado pela presença de `$GITHUB_ACTIONS`.
pub fn rodando_runner_github() -> bool {
    env::var_os("GITHUB_ACTIONS").is_some()
}

/// Publica um output no formato aceito pelo GitHub Actions.
///
/// - Dentro da runner: escreve em `$GITHUB_OUTPUT` no formato `key=value`.
/// - Fora da runner: escreve `key=value` no stdout, para manter o mesmo
///   comportamento observável em desenvolvimento local.
///
/// O arquivo é aberto em modo append porque a runner compartilha o mesmo
/// arquivo entre todos os steps do job.
pub fn escreve_output(key: &str, value: &str) -> Result<()> {
    match env::var_os("GITHUB_OUTPUT") {
        Some(path) => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| anyhow!("falha ao abrir $GITHUB_OUTPUT: {e}"))?;
            writeln!(file, "{key}={value}")
                .map_err(|e| anyhow!("falha ao escrever em $GITHUB_OUTPUT: {e}"))?;
        }
        None => {
            println!("{key}={value}");
        }
    }
    Ok(())
}

/// Emite uma anotação nativa do GitHub Actions.
///
/// `level` deve ser `"error"`, `"warning"` ou `"notice"`. Dentro da
/// runner, a mensagem aparece anexada ao step na interface. Fora da
/// runner, vai para stderr com prefixo `[level]`.
pub fn emite_anotacao(level: &str, message: &str) {
    if rodando_runner_github() {
        // No runner, o protocolo do GitHub exige que a anotação vá para stdout.
        println!("::{level}::{message}");
    } else {
        // Fora da runner, anotação é diagnóstico — convencionalmente stderr.
        // Isso mantém o stdout limpo para outputs (`escreve_output`).
        eprintln!("[{level}] {message}");
    }
}

/// Pede ao GitHub Actions para mascarar `value` em qualquer saída futura
/// dos logs. Idempotente e seguro fora da runner (não faz nada).
pub fn mascara_env_logs(value: &str) {
    if value.is_empty() || !rodando_runner_github() {
        return;
    }
    println!("::add-mask::{value}");
}

/// Lê uma variável de ambiente obrigatória.
///
/// Retorna erro com o nome da variável ausente no texto, o que ajuda a
/// diagnosticar rapidamente qual configuração faltou no pipeline.
pub fn env_obrigatoria(key: &str) -> Result<String> {
    env::var(key).map_err(|_| anyhow!("variável de ambiente obrigatória não definida: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `std::env::set_var`/`remove_var` são `unsafe` desde Rust 1.86
    // (edição 2024). Serializamos os testes de ambiente com este mutex
    // para evitar corrida entre threads de teste do Cargo.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_obrigatoria_retorna_valor_quando_existe() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: mutação protegida por ENV_LOCK; nenhuma outra thread
        // de teste lê/escreve esta variável em paralelo.
        unsafe { std::env::set_var("TOOLCORE_TEST_PRESENT", "abc") };
        assert_eq!(env_obrigatoria("TOOLCORE_TEST_PRESENT").unwrap(), "abc");
        unsafe { std::env::remove_var("TOOLCORE_TEST_PRESENT") };
    }

    #[test]
    fn env_obrigatoria_retorna_erro_quando_ausente() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("TOOLCORE_TEST_ABSENT") };
        let err = env_obrigatoria("TOOLCORE_TEST_ABSENT").unwrap_err();
        assert!(
            err.to_string().contains("TOOLCORE_TEST_ABSENT"),
            "mensagem de erro deve citar a variável ausente"
        );
    }

    #[test]
    fn escreve_output_usa_stdout_sem_github_output() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("GITHUB_OUTPUT") };
        // Apenas garante que não panica; o valor é impresso no stdout.
        escreve_output("k", "v").unwrap();
    }

    #[test]
    fn mascara_env_logs_eh_noop_fora_da_runner() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("GITHUB_ACTIONS") };
        mascara_env_logs("segredo"); // não deve imprimir nada
        mascara_env_logs(""); // vazio também não faz nada
    }

    #[test]
    fn rodando_runner_github_detecta_variavel() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("GITHUB_ACTIONS") };
        assert!(!rodando_runner_github());

        unsafe { std::env::set_var("GITHUB_ACTIONS", "true") };
        assert!(rodando_runner_github());

        unsafe { std::env::remove_var("GITHUB_ACTIONS") };
    }
}
