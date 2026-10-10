# wt

[English](README.md) · Português (Brasil)

Uma git worktree com bancos e porta web próprios, para qualquer app. `wt up feat/x` cria a worktree, clona os bancos de desenvolvimento do checkout principal (Postgres, MySQL/MariaDB, Redis/Valkey, SQLite), sobe cada um numa porta livre e escreve o `.env` da worktree apontando para eles. `wt down` desfaz tudo e mantém o branch.

## Por que isso existe

Duas branches no mesmo banco se atropelam: a migration de uma quebra a outra, os seeds se misturam, a suíte de testes paralela de uma derruba a da outra. Uma worktree resolve o código, mas não o banco. O `wt` dá a cada worktree uma cópia byte a byte do banco de desenvolvimento, isolada, em segundos (no APFS, com clonefile, ela não custa disco até alguém escrever nela).

O que muda de app para app fica num `.wt.toml`; o que dá para descobrir sozinho (imagem, comando, credenciais, volume) é lido do container de origem.

## Instalação

```bash
cargo install --path . --root ~/.local   # instala ~/.local/bin/wt
```

Requer `git` e `docker` (Docker Desktop, OrbStack ou colima). Usa, se estiverem instalados: Herdr, `direnv` e `sqlite3`.

## Quick start

```bash
cd ~/Projects/minha-app            # o checkout principal, com o docker compose rodando
wt init                            # gera o .wt.toml a partir dos containers do compose
wt up feat/x                       # worktree + bancos clonados + porta web
cd ~/.herdr/worktrees/minha-app/feat-x && source .envrc
bin/dev                            # ou o que a app usar; lê PORT e DB_PORT do ambiente/.env
wt ls                              # lista as worktrees e o estado de cada uma
wt down feat-x                     # remove containers, volumes e checkout; o branch fica
```

## Comandos

### `wt up <branch> [--base REF]`

Roda do checkout principal. Um branch existente é feito checkout; um desconhecido é criado a partir de `--base` (padrão `HEAD`). Para um branch que já existe, `--base` é ignorado com um aviso. Rodar de novo é seguro: reaproveita a worktree, as portas e os volumes já clonados, e preserva o que você editou no `.env` da worktree.

Passos, nesta ordem:

1. Cria a worktree em `<root>/<slug>` (via Herdr, se disponível; senão `git worktree add`). O slug é o branch em minúsculas, com qualquer outro caractere virando `-`, até 40 caracteres: `feat/ENG-12_x` → `feat-eng-12-x`. Um branch cujo slug já pertence à worktree de outro branch (`feat/x` e `feat-x`) é recusado.
2. Escolhe as portas: a porta web e uma por serviço, cada uma na sua faixa, pulando as que algum container (mesmo parado) publica, as que outra worktree de qualquer projeto já recebeu e as que têm algo escutando.
3. Escreve o `.env` da worktree (copiado do principal na primeira vez) e o `.envrc`.
4. Copia os arquivos SQLite e os caminhos de `app.copy`.
5. Roda `hooks.setup`.
6. Para cada serviço: clona o volume, sobe o container, espera ficar pronto e confere que os dados vieram.
7. Roda `hooks.after_services`.

### `wt down [slug] [--keep-checkout] [--yes] [--force]`

Remove os containers e volumes da worktree, o checkout e o registro de portas. O branch nunca é apagado. Sem slug, usa a worktree em que você está (e pede para sair dela) ou mostra uma lista para escolher.

Proteções:

- Só remove recursos com as labels `wt.project` e `wt.slug` desta worktree. Um container ou volume que só tem o nome esperado é recusado, a menos que você passe `--force`.
- Nunca remove um container de origem nem um volume montado nele.
- Recusa um checkout com mudanças não commitadas (`--force` passa por cima).
- Pede confirmação. Sem terminal, exige `--yes` (ou `WT_YES=1`).

`--keep-checkout` remove só os containers e volumes; a worktree e a porta web ficam.

### `wt ls`

```
SLUG    BRANCH  WEB   POSTGRES  REDIS  STATUS   DISK
feat-x  feat/x  3001  5433      6380   running  47.69MB+109B
```

| STATUS | Significado |
|---|---|
| `running` | todos os containers de pé |
| `partial` | parte dos containers de pé |
| `stopped` | containers criados, nenhum rodando |
| `no-db` | checkout sem containers (`wt up <branch>` cria) |
| `ready` | projeto sem serviços, checkout presente |
| `orphan` | sobraram recursos, mas o checkout sumiu (`wt down <slug>`) |

### `wt init [--force]`

Escreve um `.wt.toml` a partir dos containers do docker compose deste repo (identificados pela imagem) e dos arquivos do repo: `Gemfile` → `bundle install`; `package.json` com `bun.lock`, `pnpm-lock.yaml`, `yarn.lock` ou nenhum deles → o install correspondente; `bin/rails` → `bin/rails db:prepare`; `*.sqlite3` ignorados pelo git em `storage/` e `db/` (exceto os de teste); `storage/` com mais do que o `.keep` → `app.copy`. Não sobrescreve um `.wt.toml` existente sem `--force`. Revise o arquivo antes do primeiro `wt up`.

## O `.wt.toml`

O `wt` procura, nesta ordem: o caminho em `WT_CONFIG`, `<checkout principal>/.wt.toml` e `~/.config/wt/<project>.toml` (para repos onde você não pode commitar o arquivo). Sem nenhum, `wt up` cria só a worktree, a porta web e o `.env`.

Todas as opções, com os padrões:

```toml
project = "<nome do diretório do checkout principal, normalizado como o compose faz>"
root = "~/.herdr/worktrees/<project>"     # onde as worktrees ficam

[app]
port_env = "PORT"           # chave que recebe a porta web; "" desliga a porta web
port_range = [3001, 3099]
env_file = ".env"           # copiado do principal na primeira vez, depois reescrito no lugar (0600)
envrc = true                # escreve .envrc exportando a porta web (pulado se o repo versiona um .envrc)
copy = []                   # caminhos não versionados copiados do principal

[services.postgres]         # as mesmas opções valem para [services.mysql] e [services.redis]
source = "<project>-postgres-1"   # o container de origem; o padrão é o nome que o compose dá
volume = "<detectado>"            # o volume de dados do source
clone = true                      # false: volume vazio, mesma imagem/comando/env
port_range = [5433, 5499]
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}" }
database = "postgres"             # só postgres: o banco usado para conferir o clone

[services.sqlite]
files = []                  # obrigatório: arquivos relativos à raiz do repo

[hooks]
setup = []                  # antes dos serviços
after_services = []         # depois que os serviços estão prontos
```

### Serviços

A chave de `[services.<tipo>]` é o próprio tipo: `postgres`, `mysql`, `redis` ou `sqlite`. Qualquer outra é erro, e cada app tem no máximo um serviço de cada tipo. Uma tabela vazia, como `[services.postgres]`, basta quando o compose segue o padrão.

O clone não repete nada do compose no config: imagem, entrypoint, comando, variáveis de ambiente, usuário, diretório de trabalho, porta interna e diretório de dados são lidos do container de origem com `docker inspect`, então o clone sempre sobe exatamente como a origem. O clone também recebe os bind mounts da origem (arquivos de config, scripts de init), sempre somente-leitura, e os mounts tmpfs. Os outros volumes nomeados e qualquer coisa montada dentro do diretório de dados ficam de fora, para o clone nunca escrever em estado que pertence à origem. O volume de dados é o montado no diretório de dados padrão do tipo; se não houver, o único volume do container; se houver mais de um e nenhum no lugar padrão, use `volume`. Dados num bind mount não são suportados. Quando a origem publica várias portas (3306 e 33060 do MySQL), o clone usa a porta padrão do tipo se ela estiver entre elas; senão, a menor porta TCP.

| | Postgres | MySQL/MariaDB | Redis/Valkey | SQLite |
|---|---|---|---|---|
| Imagens (`wt init`) | `postgres`, `postgis`, `pgvector`, `timescaledb` | `mysql`, `mariadb`, `percona` | `redis`, `valkey`, `keydb`, `redis-stack-server` | arquivos `*.sqlite3` |
| Diretório de dados | `/var/lib/postgresql/data`, `/var/lib/postgresql` (18+) | `/var/lib/mysql` | `/data` | — |
| Faixa de portas | 5433–5499 | 3307–3399 | 6380–6449 | — |
| `env` padrão | `DB_HOST`, `DB_PORT` | `DB_HOST`, `DB_PORT` | `REDIS_URL=redis://127.0.0.1:{port}/0` | — |
| Pronto quando | `pg_isready` por TCP | `mysqladmin ping` | `redis-cli ping` | — |
| Conferência | `select 1` em `database` | ping | ping | — |

Em `env`, `{port}` vira a porta do clone no host. Os valores usam `127.0.0.1`, não `localhost`: o clone publica só em IPv4, e o macOS resolve `localhost` para `::1` primeiro.

As faixas não se sobrepõem entre si nem com a porta web, e ficam todas acima da porta padrão de cada banco, para que um clone nunca ocupe a porta do stack principal.

### O banco principal durante o clone

Uma cópia byte a byte precisa do banco parado: um diretório de dados vivo tem páginas pela metade. Por isso o `wt up` para o container de origem (`docker stop`), copia o volume e o liga de novo. Ele fica fora do ar só pelos segundos da cópia, e volta mesmo se a cópia falhar ou você apertar Ctrl-C. Isso só acontece na primeira vez: um volume já clonado é reaproveitado nos re-runs. Um volume com a cópia pela metade (sem o marcador `.wt-clone-complete`) é descartado e copiado de novo. Se a própria checagem do marcador não conseguir rodar, o `wt up` para com erro e não mexe no volume.

O SQLite não para nada: `sqlite3 .backup` tira uma cópia consistente mesmo com a app usando o arquivo. Sem o CLI `sqlite3`, o `wt` copia o arquivo e o `-wal`/`-shm`. Cada arquivo é copiado uma vez só; apague o da worktree para copiar de novo.

### `app.copy`

Copia do checkout principal o que ainda não existe na worktree, e nunca sobrescreve: um arquivo criado ou alterado dentro da worktree sobrevive aos re-runs. No APFS a cópia é um clonefile, então um diretório grande não custa disco até alguém escrever nele. O caso típico são os blobs do Active Storage em `storage/`, para os quais o banco clonado aponta.

### Hooks

Cada hook é uma lista de argumentos, executada direto, sem shell, no diretório da worktree. Eles rodam em ordem, e o primeiro que falhar interrompe o `wt up`.

```toml
[hooks]
setup = [["bundle", "install"], ["bun", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

Sem shell, não há pipes, `||`, globs nem `$VAR`. Para isso, aponte para um script do repo (`["bin/setup"]`) ou chame o shell explicitamente (`["sh", "-c", "…"]`).

Os hooks recebem no ambiente: `WT_SLUG`, `WT_BRANCH`, `WT_PATH`, `WT_MAIN_ROOT`, `WT_APP_PORT` e `WT_POSTGRES_PORT`/`WT_MYSQL_PORT`/`WT_REDIS_PORT` (só dos serviços configurados).

## Exemplos

**Rails com Postgres** ([`examples/rails-postgres.wt.toml`](examples/rails-postgres.wt.toml)): Postgres no compose e Active Storage em disco.

```toml
[app]
copy = ["storage"]

[services.postgres]

[hooks]
setup = [["bundle", "install"], ["bun", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

**Rails 8 com SQLite:** sem Docker.

```toml
[app]
copy = ["storage"]

[services.sqlite]
files = ["storage/development.sqlite3"]

[hooks]
setup = [["bundle", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

**Node com MySQL e Redis:** a app lê `DATABASE_URL`, e o cache começa vazio.

```toml
[services.mysql]
env = { DATABASE_URL = "mysql://root:root@127.0.0.1:{port}/app" }

[services.redis]
clone = false

[hooks]
setup = [["pnpm", "install"]]
after_services = [["pnpm", "prisma", "migrate", "deploy"]]
```

## O que fica onde

| O quê | Onde |
|---|---|
| Worktree | `<root>/<slug>` (padrão `~/.herdr/worktrees/<project>/<slug>`) |
| Containers | `<project>-wt-<slug>-<tipo>`, com as labels `wt.project`, `wt.slug`, `wt.service` |
| Volumes | `<project>-wt-<slug>-<tipo>-data`, com as mesmas labels |
| Portas | `~/.local/state/wt/<project>/<slug>.toml` (respeita `XDG_STATE_HOME`) |

As portas ficam fora do checkout para não aparecerem no `git status`. Esse arquivo é o que um re-run reaproveita e o que impede duas worktrees, criadas com os servidores parados, de receberem a mesma porta web.

## Integrações

**Herdr.** Se o `herdr` estiver instalado e o servidor responder, o `wt` cria e remove worktrees por ele (fechando o workspace aberto, se houver). O `root` padrão é o diretório de worktrees do Herdr. `WT_NO_HERDR=1` força o git puro, e `WT_TRUST_REPOSITORY=1` repassa `--trust-repository` ao Herdr.

**direnv.** O `.envrc` exporta só a porta web, porque muitos servidores (o `bin/rails server` e o `bin/dev`, por exemplo) leem `PORT` do ambiente antes de carregar o `.env`. O `wt` roda `direnv allow` a cada reescrita. Ele não exporta a porta do banco de propósito: um shell que deu `source .envrc` à mão continuaria com ela ao voltar para o checkout principal, apontando-o para o banco da worktree.

**Sessões no navegador.** Cookies ignoram a porta, então duas worktrees em `localhost` compartilham a sessão e uma desloga a outra. Abra cada uma no seu subdomínio: `http://feat-x.localhost:3001`.

## Variáveis de ambiente

| Variável | Efeito |
|---|---|
| `WT_CONFIG` | caminho explícito do `.wt.toml` |
| `WT_NO_HERDR=1` | usa `git worktree` mesmo com o Herdr disponível |
| `WT_TRUST_REPOSITORY=1` | passa `--trust-repository` ao `herdr worktree create` |
| `WT_YES=1` | `wt down` sem confirmação |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | onde ficam o config fora do repo e o registro de portas |

## Desenvolvimento

```bash
cargo test                          # testes unitários
cargo clippy --all-targets -- -D warnings
tests/e2e.sh                        # ciclo completo contra um compose descartável (Postgres + Redis + SQLite)
```

## Licença

[MIT](LICENSE)
