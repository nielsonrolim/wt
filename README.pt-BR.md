# wt

[English](README.md) · Português (Brasil)

Uma git worktree com bancos e porta web próprios, para qualquer app. `wt up feat/x` cria a worktree e clona os bancos de desenvolvimento do checkout principal (Postgres, MySQL/MariaDB, Redis/Valkey, SQLite). Por padrão cada clone ganha o próprio container; com `mode = "database"` a worktree ganha bancos lógicos próprios dentro da instância principal, sem container nenhum. O `wt` sobe cada clone numa porta livre e escreve o `.env` da worktree apontando para eles. `wt down` desfaz tudo, perguntando sobre o branch e mantendo-o por padrão.

## Por que isso existe

Duas branches no mesmo banco se atropelam: a migration de uma quebra a outra, os seeds se misturam, a suíte de testes paralela de uma derruba a da outra. Uma worktree resolve o código, mas não o banco. O `wt` dá a cada worktree uma cópia byte a byte do banco de desenvolvimento, isolada, em segundos (no APFS, com clonefile, ela não custa disco até alguém escrever nela). Onde um container é exagero, `mode = "database"` copia os bancos dentro da instância que já está rodando.

O que muda de app para app fica num `.wt.toml`; o que dá para descobrir sozinho (imagem, comando, credenciais, volume) é lido do container de origem.

## Instalação

Binários prontos para Linux x86_64 e macOS (Intel e Apple Silicon):

```bash
curl -fsSL https://raw.githubusercontent.com/nielsonrolim/wt/main/install.sh | sh
```

O script baixa o último release, confere o SHA-256 e instala `~/.local/bin/wt`. `WT_VERSION=v0.3.0` fixa um release e `WT_INSTALL_DIR` muda onde o binário vai.

Com Rust:

```bash
cargo binstall --git https://github.com/nielsonrolim/wt wt   # baixa o binário pronto
cargo install --git https://github.com/nielsonrolim/wt --locked   # compila do código
cargo install --path . --root ~/.local   # a partir de um clone; instala ~/.local/bin/wt
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
wt down feat-x                     # remove containers, volumes e checkout; o branch fica, a menos que você peça
```

## Comandos

### `wt up <branch> [--base REF]`

Roda do checkout principal. Um branch existente é feito checkout; um desconhecido é criado a partir de `--base` (padrão `HEAD`). Para um branch que já existe, `--base` é ignorado com um aviso. Rodar de novo é seguro: reaproveita a worktree, as portas e os volumes já clonados, e preserva o que você editou no `.env` da worktree.

Passos, nesta ordem:

1. Cria a worktree em `<root>/<slug>` (via Herdr, se disponível; senão `git worktree add`). O slug é o branch em minúsculas, com qualquer outro caractere virando `-`, até 40 caracteres: `feat/ENG-12_x` → `feat-eng-12-x`. Um branch cujo slug já pertence à worktree de outro branch (`feat/x` e `feat-x`) é recusado.
2. Escolhe as portas: a porta web e uma por serviço, cada uma na sua faixa, pulando as que algum container (mesmo parado) publica, as que outra worktree de qualquer projeto já recebeu e as que têm algo escutando. Um serviço em `mode = "database"` não tem porta própria: usa a porta publicada da origem.
3. Escreve o `.env` da worktree (copiado do principal na primeira vez) e o `.envrc`.
4. Copia os arquivos SQLite e os caminhos de `app.copy`, e atualiza os de `app.refresh`.
5. Roda `hooks.setup`.
6. Para cada serviço: clona o volume e sobe o container ou, com `mode = "database"`, copia os bancos lógicos dentro da instância de origem. Depois espera ficar pronto e confere que os dados vieram.
7. Roda `hooks.after_services`.

### `wt down [slug] [--keep-checkout] [--delete-branch] [--yes] [--force]`

Remove os containers e volumes da worktree, os bancos que ela copiou dentro da instância de origem, o checkout e o registro de portas. Na hora de tirar o checkout, pergunta se o branch também deve ser apagado, e o padrão é mantê-lo. Sem slug, usa a worktree em que você está (e pede para sair dela) ou mostra uma lista para escolher.

Proteções:

- Só remove recursos com as labels `wt.project` e `wt.slug` desta worktree (ou a de `docker.slug_label`, se configurada). Um container ou volume que só tem o nome esperado é recusado, a menos que você passe `--force`.
- Só dropa um banco copiado quando o marcador dele nomeia este projeto e slug; um banco que só tem o nome esperado é recusado sem `--force`. Nunca dropa o banco de origem.
- Nunca remove um container de origem nem um volume montado nele.
- Recusa um checkout com mudanças não commitadas (`--force` passa por cima).
- Pede confirmação e, depois, sobre o branch. Sem terminal, exige `--yes` (ou `WT_YES=1`), que responde tudo com o padrão: mantém o branch.
- Só oferece o branch quando o checkout desta worktree vai embora, e o apaga depois que o checkout sumiu. Um branch ainda em checkout em outro lugar — o principal incluído — nunca é tocado.
- Se o Docker estiver inalcançável, ou a origem parada e você passar `--force`, os bancos copiados não podem ser dropados: o `down` remove o checkout mas mantém o arquivo de estado, para uma execução posterior terminar o serviço. Uma origem parada sem `--force` faz o `down` parar com erro. O `wt` nunca inicia a origem.

`--keep-checkout` remove os containers, volumes e bancos copiados; a worktree e a porta web ficam, então não há branch sobre o que perguntar.

`--delete-branch` pula a pergunta e apaga o branch depois que o checkout sumiu, perdendo commits que nunca foram mergeados em lugar nenhum. Não dá para combinar com `--keep-checkout`.

### `wt ls`

```
SLUG    BRANCH  WEB   POSTGRES  REDIS  STATUS   DISK
feat-x  feat/x  3001  5433      6380   running  47.69MB+109B
```

| STATUS | Significado |
|---|---|
| `running` | todos os containers de pé |
| `partial` | parte dos containers de pé, ou parte dos bancos copiados registrados sumiu |
| `stopped` | containers criados, nenhum rodando |
| `shared` | sem container; todos os bancos copiados registrados no estado existem |
| `no-db` | checkout sem containers nem bancos copiados (`wt up <branch>` cria) |
| `ready` | projeto sem serviços, checkout presente |
| `unknown` | Docker inalcançável, ou uma instância de origem parada, e a worktree tem bancos copiados, então o `ls` não sabe dizer |
| `orphan` | sobraram recursos, mas o checkout sumiu (`wt down <slug>`) |

`orphan` vem do sistema de arquivos e vence `unknown`; um container da worktree vence o veredito de banco, e uma origem parada reporta `unknown` em vez de `no-db`. O `wt ls` nunca muda nada.

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
copy = []                   # caminhos não versionados copiados do principal, nunca sobrescritos
refresh = []                # como copy, mas substituídos sempre que o arquivo do principal for diferente

[services.postgres]         # as mesmas opções valem para [services.mysql] e [services.redis]
source = "<project>-postgres-1"   # o container de origem; o padrão é o nome que o compose dá
mode = "container"                # "database": copia bancos lógicos dentro da instância de origem
# databases = ["<origem>"]        # mode = "database": todos os bancos de origem a copiar; `database` é o atalho de um item
volume = "<detectado>"            # o volume de dados do source (só em mode = "container")
clone = true                      # false: volume vazio, ou bancos vazios no modo database
port_range = [5433, 5499]         # só em mode = "container"; o modo database usa a porta da origem
env = { DB_HOST = "127.0.0.1", DB_PORT = "{port}" }
database = "postgres"             # postgres em container mode: o alvo da conferência; no modo database, o atalho de um item
check_sql = "select 1"            # só postgres: a query dessa conferência; exige database em container mode e roda no primário no modo database

[services.sqlite]
files = []                  # obrigatório: arquivos relativos à raiz do repo

[docker]
legacy_names = false        # true: o clone do Postgres é <project>-wt-<slug> e o volume <project>-wt-<slug>-db
# slug_label = "myapp.worktree.slug"   # label extra com o slug; não definida por padrão

[hooks]
setup = []                  # antes dos serviços
after_services = []         # depois que os serviços estão prontos
```

### Serviços

A chave de `[services.<tipo>]` é o próprio tipo: `postgres`, `mysql`, `redis` ou `sqlite`. Qualquer outra é erro, e cada app tem no máximo um serviço de cada tipo. Uma tabela vazia, como `[services.postgres]`, basta quando o compose segue o padrão.

O clone não repete nada do compose no config: imagem, entrypoint, comando, variáveis de ambiente, usuário, diretório de trabalho, porta interna, diretório de dados, tamanho da memória compartilhada e ulimits são lidos do container de origem com `docker inspect`, então o clone sempre sobe exatamente como a origem. O clone também recebe os bind mounts da origem (arquivos de config, scripts de init), sempre somente-leitura, e os mounts tmpfs. Os outros volumes nomeados e qualquer coisa montada dentro do diretório de dados ficam de fora, para o clone nunca escrever em estado que pertence à origem. O volume de dados é o montado no diretório de dados padrão do tipo; se não houver, o único volume do container; se houver mais de um e nenhum no lugar padrão, use `volume`. Dados num bind mount não são suportados. Dados divididos entre mounts também não: o `wt up` recusa uma origem com algo montado dentro do diretório de dados (um bind de `pg_wal`, por exemplo) ou um Postgres com `POSTGRES_INITDB_WALDIR`, porque só o volume de dados é copiado e o clone subiria sem o resto. Use `clone = false` para dar a esse serviço um banco vazio. Quando a origem publica várias portas (3306 e 33060 do MySQL), o clone usa a porta padrão do tipo se ela estiver entre elas; senão, a menor porta TCP.

### Modo database

`mode = "database"` dá à worktree bancos lógicos próprios dentro da instância de origem, em vez de um container: nenhuma imagem sobe, nenhum volume é copiado e a origem nunca é parada. Vale para Postgres e MySQL/MariaDB. O Redis não tem bancos separados para copiar, e o SQLite já é um arquivo só: a cópia dele é o banco inteiro.

```toml
[services.postgres]
mode = "database"
databases = ["myapp_development", "myapp_cache"]
env = {
  DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}",
  CACHE_URL    = "postgres://app:app@127.0.0.1:{port}/{database:myapp_cache}",
}
```

- `databases` lista todos os bancos de origem a copiar. `database = "x"` é o atalho de um item só; os dois juntos são erro. Com nenhum dos dois, o `wt` copia o único banco que o próprio env da imagem nomeia (`POSTGRES_DB`/`POSTGRES_USER`, `MYSQL_DATABASE`/`MARIADB_DATABASE`). Todos são copiados, então uma app com vários bancos lógicos não precisa de serviço extra.
- O primeiro item é o primário: `{database}` é a cópia dele, e `check_sql` roda nele. `{database:<origem>}` é a cópia de outro item. Pelo menos um valor de `env` neste modo precisa usar `{database}` ou `{database:<origem>}`, senão a worktree escreveria na origem; sem isso a config é recusada.
- Cada banco de origem vira `wt_<project>_<slug>__<origem>`, sanitizado para `[a-z0-9_]` e com no máximo 63 bytes, com hash determinístico quando passaria disso. O nome não depende da ordem da lista, e dois nomes que caem no mesmo alvo são recusados.
- A posse é o próprio banco: uma cópia no Postgres carrega `wt.project`, `wt.slug` e `wt.complete` no comentário do banco; uma cópia no MySQL carrega isso numa tabela `__wt_meta`. Banco sem esse marcador é recusado pelo `up` e pelo `down` (a menos que `--force`), e o banco de origem nunca é dropado.
- A cópia é um dump escrito num arquivo dentro do container e restaurado dele, então um dump que falhou não se confunde com uma cópia pronta. Postgres usa `pg_dump`/`psql`; MySQL usa `mysqldump`/`mysql` com `--single-transaction --routines --triggers --events` (`--set-gtid-purged=OFF` só no MySQL, não no MariaDB), e concede o usuário da app no banco novo.
- Re-runs reaproveitam uma cópia pronta. Um banco copiado pela metade (marcador `complete=0`) é dropado e copiado de novo, e só ele.
- O `down` dropa todas as cópias registradas no estado, incluindo a de um banco que saiu de `databases` (o `up` avisa e mantém). `--keep-checkout` dropa as cópias e mantém o checkout.
- Trocar o modo de uma worktree é recusado, não migrado: o `up` para com erro e aponta `wt down <slug>` quando um container ou volume nosso (indo para o modo database) ou uma cópia registrada (voltando para container) está no caminho.
- A origem precisa publicar a porta: a worktree conecta em `127.0.0.1:<porta publicada>`. Uma porta de host efêmera muda quando o container é recriado; publique uma fixa se worktrees duradouras dependerem dela. Uma worktree mantém a porta registrada até o próximo `wt up`, que lê a nova porta publicada e reescreve o `.env`.

Limitações: as cópias são independentes, então referências entre bancos (uma view MySQL citando outro schema, FDW ou dblink no Postgres) continuam apontando para a origem. Roles são globais na instância. Não há clonefile: uma cópia lógica ocupa disco de verdade. Tabelas MyISAM não são transacionais, e DDL concorrente pode esperar o dump.

| | Postgres | MySQL/MariaDB | Redis/Valkey | SQLite |
|---|---|---|---|---|
| Imagens (`wt init`) | `postgres`, `postgis`, `pgvector`, `timescaledb` | `mysql`, `mariadb`, `percona` | `redis`, `valkey`, `keydb`, `redis-stack`, `redis-stack-server` | arquivos `*.sqlite3` |
| Diretório de dados | `/var/lib/postgresql/data`, `/var/lib/postgresql` (18+) | `/var/lib/mysql` | `/data` | — |
| Faixa de portas | 5433–5499 | 3307–3399 | 6380–6449 | — |
| `env` padrão | `DB_HOST`, `DB_PORT` | `DB_HOST`, `DB_PORT` | `REDIS_URL=redis://127.0.0.1:{port}/0` | — |
| Pronto quando | `pg_isready` por TCP | `mysqladmin ping` | `redis-cli ping` | — |
| Conferência | `select 1` em `database`, ou `check_sql` | ping | ping | — |
| Modo database | sim | sim | não (não há bancos separados) | n/a (a cópia do arquivo é o banco) |

Em `env`, `{port}` vira a porta do clone no host. Os valores usam `127.0.0.1`, não `localhost`: o clone publica só em IPv4, e o macOS resolve `localhost` para `::1` primeiro.

As faixas não se sobrepõem entre si nem com a porta web, e ficam todas acima da porta padrão de cada banco, para que um clone nunca ocupe a porta do stack principal.

### O banco principal durante o clone

Uma cópia byte a byte precisa do banco parado: um diretório de dados vivo tem páginas pela metade. Por isso o `wt up` para o container de origem (`docker stop`), copia o volume e o liga de novo. Ele fica fora do ar só pelos segundos da cópia, e volta mesmo se a cópia falhar ou você apertar Ctrl-C. Isso só acontece na primeira vez: um volume já clonado é reaproveitado nos re-runs. Um volume com a cópia pela metade (sem o marcador `.wt-clone-complete` nem o `.worktree-clone-complete` que scripts no estilo do `bin/worktree` escrevem) é descartado e copiado de novo. Se a própria checagem do marcador não conseguir rodar, o `wt up` para com erro e não mexe no volume. Um serviço em `mode = "database"` nunca para a origem: ele tira um dump consistente de cada banco com o stack rodando.

O SQLite não para nada: `sqlite3 .backup` tira uma cópia consistente mesmo com a app usando o arquivo. Sem o CLI `sqlite3`, o `wt` copia o arquivo e o `-wal`/`-shm`. Cada arquivo é copiado uma vez só; apague o da worktree para copiar de novo.

### `app.copy`

Copia do checkout principal o que ainda não existe na worktree, e nunca sobrescreve: um arquivo criado ou alterado dentro da worktree sobrevive aos re-runs. No APFS a cópia é um clonefile, então um diretório grande não custa disco até alguém escrever nele. O caso típico são os blobs do Active Storage em `storage/`, para os quais o banco clonado aponta.

### `app.refresh`

Como `app.copy`, mas um arquivo que mudou no checkout principal substitui a cópia da worktree a cada `wt up`. Serve para arquivos não versionados que você edita no principal e quer que todas as worktrees acompanhem, como definições de agentes e skills ignoradas pelo git. Um arquivo que só a worktree tem fica como está, e um idêntico não é tocado. Um caminho não pode estar em `copy` e em `refresh` ao mesmo tempo.

### `[docker]`

Para um repo que já tem ambientes de worktree feitos por outra ferramenta: o `wt` os assume em vez de subir um segundo banco ao lado de cada um.

- `legacy_names = true` chama o clone do Postgres de `<project>-wt-<slug>` e o volume dele de `<project>-wt-<slug>-db`. MySQL e Redis mantêm o serviço no nome.
- `slug_label` é uma label extra que o `up` grava com o slug em todo container e volume. `down` e `ls` a aceitam como prova de posse, junto com `wt.project` e `wt.slug`, então os ambientes que a outra ferramenta marcou com ela aparecem no `ls` e podem ser removidos. O nome ainda precisa estar no namespace `<project>-wt-`.

`wt up` num ambiente existente reaproveita o container e o volume, e a porta publicada vale mais que o arquivo de estado.

### Hooks

Cada hook é uma lista de argumentos, executada direto, sem shell, no diretório da worktree. Eles rodam em ordem, e o primeiro que falhar interrompe o `wt up`.

```toml
[hooks]
setup = [["bundle", "install"], ["bun", "install"]]
after_services = [["bin/rails", "db:prepare"]]
```

Sem shell, não há pipes, `||`, globs nem `$VAR`. Para isso, aponte para um script do repo (`["bin/setup"]`) ou chame o shell explicitamente (`["sh", "-c", "…"]`).

Os hooks recebem no ambiente: `WT_SLUG`, `WT_BRANCH`, `WT_PATH`, `WT_MAIN_ROOT`, `WT_APP_PORT` e `WT_POSTGRES_PORT`/`WT_MYSQL_PORT`/`WT_REDIS_PORT` (só dos serviços configurados). Em `mode = "database"`, recebem também `WT_POSTGRES_DATABASE`/`WT_MYSQL_DATABASE` (o nome da cópia primária) e `WT_POSTGRES_DATABASES`/`WT_MYSQL_DATABASES` (`origem=alvo,origem=alvo`).

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

**Rails com Postgres, compartilhando a instância principal** ([`examples/rails-postgres-database.wt.toml`](examples/rails-postgres-database.wt.toml)): sem container por worktree; cada uma ganha bancos lógicos próprios dentro do Postgres do compose.

```toml
[app]
copy = ["storage"]

[services.postgres]
mode = "database"
databases = ["myapp_development", "myapp_cache"]
env = {
  DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database}",
  CACHE_DATABASE_URL = "postgres://app:app@127.0.0.1:{port}/{database:myapp_cache}",
}

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
| Containers | `<project>-wt-<slug>-<tipo>`, com as labels `wt.project`, `wt.slug`, `wt.service` (e `slug_label`, se definida) |
| Volumes | `<project>-wt-<slug>-<tipo>-data`, com as mesmas labels |
| Bancos copiados (`mode = "database"`) | `wt_<project>_<slug>__<origem>`, dentro da instância de origem, provados pelo comentário / `__wt_meta` e registrados no estado |
| Portas | `~/.local/state/wt/<project>/<slug>.toml` (respeita `XDG_STATE_HOME`) |

Com `docker.legacy_names`, o Postgres é `<project>-wt-<slug>` e o volume dele `<project>-wt-<slug>-db`.

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
| `WT_YES=1` | `wt down` sem confirmação; o branch é mantido |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | onde ficam o config fora do repo e o registro de portas |

## Desenvolvimento

```bash
cargo test                          # testes unitários
cargo clippy --all-targets -- -D warnings
tests/e2e.sh                        # ciclo completo contra um compose descartável (Postgres + MySQL + Redis + SQLite), nos dois modos
```

## Licença

[MIT](LICENSE)
