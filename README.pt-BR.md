[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Tradução para o português (Brasil) do [README.md](README.md). O texto original em inglês é a versão oficial e prevalece em caso de divergência.*

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="web/public/mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="web/public/mark-light.svg">
    <img src="web/public/mark-dark.svg" alt="Sotto" width="64" height="64">
  </picture>
</p>

# Sotto

Sincronização de segredos com criptografia de ponta a ponta para equipes de desenvolvimento. Pare de enviar seu `.env` pelo Slack.

> [!WARNING]
> O Sotto é pré-1.0 e **não** passou por uma auditoria criptográfica de terceiros. Funciona de ponta a ponta, mas
> você ainda não deve confiar a ele segredos críticos de produção. Consulte [SECURITY.md](SECURITY.md).

O Sotto se baseia em uma única implementação criptográfica em Rust compartilhada pela CLI nativa e pelo cliente
do navegador via WebAssembly. O servidor armazena e sincroniza dados criptografados sem nunca receber
segredos em texto puro nem chaves utilizáveis.

## Formas de usar o Sotto

O Sotto é um único produto de código aberto que você pode usar de três formas. Seja qual for a sua
escolha, o servidor só armazena texto cifrado.

| Modo | O que você recebe |
| --- | --- |
| Local | A CLI e um cofre criptografado na sua própria máquina. Sem conta e sem servidor. |
| Auto-hospedado | O mesmo servidor Apache-2.0 em uma infraestrutura que você opera. Ele não aplica limites de plano nem verificações do serviço hospedado; consulte [Auto-hospedagem](#auto-hospedagem). |
| Sotto Cloud | O servidor que o Sotto opera em [getsotto.co.uk](https://getsotto.co.uk), para quando você prefere não manter um. `sotto login` usa esse servidor por padrão. Os planos e limites estão na [página de preços](https://getsotto.co.uk/#pricing), e o status do serviço em [status.getsotto.co.uk](https://status.getsotto.co.uk). |

Você pode começar localmente e adicionar um servidor depois: após `sotto login`, `sotto push` envia o
ambiente ativo do jeito que está.

## Status atual

O fluxo de ponta a ponta funciona: criptografe localmente, sincronize texto cifrado, descriptografe em outro dispositivo ou no
navegador e compartilhe um único segredo com um link de uso único. Equipes também funcionam de ponta a ponta: organizações
com papéis, concessões de ambiente por membro, rotação de chaves ao remover um membro, tokens de máquina para CI,
um log de auditoria da organização e a redefinição da conta em caso de perda do Emergency Kit.

| Componente | Disponível agora |
| --- | --- |
| Núcleo criptográfico | KDF, AEAD XChaCha20-Poly1305 + AAD, encapsulamento de chaves, concessões via sealed box X25519, a hierarquia de cofres de ambiente, reencapsulamento de chaves de dados (rotação), criptografia de links de compartilhamento e codificação de chaves, com vetores de referência nativo↔WASM, fuzzing dos codecs e provas Kani para a análise de cabeçalhos de envelope e de símbolos de chave |
| CLI | Segredos locais (`init`, `set`, `get`, `ls`, `rm`, `import`, `export`, `run`), ambientes (`env ls/use/diff/copy`), um painel interativo, sincronização `login`/`push`/`pull`, `setup` para novos dispositivos, `history` e `rollback`, links de uso único (`share`, `shares ls/revoke`); equipes: `org create/ls/invite/members/remove/plan/audit`, `grant`, `clone`, `rotate`, `token create/ls/revoke` de máquina com expiração (com modo `SOTTO_TOKEN` para CI); `status`, `cloud status`, `reset` para kit de emergência perdido, temas e autocompletar do shell |
| Servidor | Login OAuth + sessões, sincronização de conta + snapshots (escritas versionadas, ETag), organizações + membros + papéis, concessões de chaves de cofre por membro, rotação transacional de chaves, tokens de máquina com expiração, histórico de segredos, um log de auditoria da organização, exclusão opcional de organizações com um período de recuperação, redefinição de conta e links de uso único - apenas texto cifrado. Uma implantação do Sotto Cloud também aplica limites de plano e pode cobrar via Stripe |
| Web | Uma página inicial e guias; o cofre em `/app`, que é desbloqueado no navegador para navegar, buscar, mostrar e copiar segredos, criar links de uso único, compartilhar um ambiente com um membro e rotacionar a chave de um ambiente; um painel de equipe com organizações, membros, convites por e-mail, o plano, o log de auditoria e a exclusão de organizações; a página de recebimento de links de uso único; e uma página de conta do Sotto Cloud em `/cloud` |

## Instalação

Binários pré-compilados e assinados para macOS (Apple Silicon + Intel), Linux (x86_64 + ARM64) e
Windows x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
```

O instalador verifica a soma de verificação SHA-256 do arquivo compactado, e sua assinatura Sigstore quando `cosign`
está instalado, antes de instalar (`~/.local/bin` no macOS/Linux, `%LOCALAPPDATA%\sotto\bin` no
Windows). Prefere conferir antes? Baixe um arquivo compactado na
[página de releases](https://github.com/getsotto/sotto/releases) e verifique manualmente conforme
[SECURITY.md](SECURITY.md), ou compile do código-fonte (consulte [Desenvolvimento](#desenvolvimento)).

Os instaladores, a fórmula do Homebrew e a action do GitHub instalam releases publicadas,
enquanto este README acompanha a branch `main`, que pode estar à frente da release mais recente. Por
isso, um comando descrito aqui pode ainda não existir na versão que você instalou. `sotto --help`
lista os comandos que a sua versão suporta.

Consulte [COMPATIBILITY.md](COMPATIBILITY.md) para ver os requisitos de versão das integrações,
as plataformas declaradas e as evidências de teste disponíveis atualmente.

### Homebrew (prévia)

O [tap do Sotto](https://github.com/getsotto/homebrew-tap) oferece outra forma
de instalação no macOS. A fórmula distribui versões revisadas da CLI e exige
macOS 15 ou posterior. Apple Silicon é a plataforma principal; Intel tem suporte
de melhor esforço e está no Tier 3 do Homebrew. O Homebrew planeja encerrar o
suporte a Intel em setembro de 2027 ou depois. Consulte as
[orientações de instalação e suporte do tap](https://github.com/getsotto/homebrew-tap#readme)
e o [registro de prontidão para lançamento](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md)
antes de usar. Evite esta prévia para segredos críticos de produção.

Instale e atualize a fórmula com:

```sh
brew install getsotto/tap/sotto
```

```sh
brew update
brew upgrade getsotto/tap/sotto
```

Se houver outra instalação do `sotto` no seu `PATH`, confira os executáveis
encontrados e o binário gerenciado pelo Homebrew antes de escolher qual usar:

```sh
which -a sotto
brew list --versions sotto
"$(brew --prefix)/bin/sotto" --version
```

Para remover apenas a fórmula do Homebrew, execute:

```sh
brew uninstall getsotto/tap/sotto
```

Desinstalar a fórmula preserva os arquivos do projeto, os dados locais do
Sotto, os itens do app Acesso às Chaves e outras instalações do `sotto`. Confirme o
binário selecionado e guarde seu Kit de Emergência antes de remover ou trocar
uma instalação existente.

### Autocompletar do shell

`sotto completions <shell>` imprime um script de autocompletar na saída padrão. Por exemplo:

```sh
# bash
mkdir -p ~/.local/share/bash-completion/completions
sotto completions bash > ~/.local/share/bash-completion/completions/sotto

# zsh (add the directory to fpath in ~/.zshrc first)
mkdir -p ~/.zfunc
sotto completions zsh > ~/.zfunc/_sotto

# fish
mkdir -p ~/.config/fish/completions
sotto completions fish > ~/.config/fish/completions/sotto.fish
```

Os tarballs e zips das releases também contêm os arquivos correspondentes `completions/sotto.*`
se você preferir copiá-los diretamente.

### GitHub Actions

Para GitHub Actions, use a [action Sotto Setup](https://github.com/getsotto/sotto-action) para
instalar uma versão exata da CLI e verificar sua soma de verificação e bundles Sigstore antes de deixar o `sotto`
disponível para as etapas seguintes:

```yaml
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
      - uses: getsotto/sotto-action@v1.1.0
        with:
          sotto-version: v0.9.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

A referência da action e `sotto-version` são independentes. Mantenha `sotto-version` como uma
versão exata `vX.Y.Z`. Defina a variável de repositório `SOTTO_SERVER` com a URL do seu servidor,
inclusive `https://getsotto.co.uk` para o Sotto Cloud: com `SOTTO_TOKEN` definido, `sotto run` e
`sotto export` nunca recorrem a um servidor padrão, então o token só vai para um servidor que você
indicou. Consulte a [documentação da action](https://github.com/getsotto/sotto-action#readme) para
exemplos de matriz, Windows e workflows reutilizáveis.

## Início rápido

Quer uma demo funcional? O [**sotto-example**](https://github.com/getsotto/sotto-example)
mostra a injeção local de segredos com um GIF de Python e passos copiáveis, além de pequenos exemplos
em JavaScript, TypeScript, Java, C#, PHP, Go, Rust e C++. Não precisa de conta; as instruções cobrem
macOS, Linux e Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto get DATABASE_URL -c    # copy a secret without printing it; clipboard clears after 45s when unchanged
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
```

`sotto init` cria os ambientes `dev`, `staging` e `prod` e seleciona `dev`. A exportação escreve
texto puro, portanto precisa de `--reveal` em um terminal, assim como `sotto get`; `--format` também
aceita `shell` e `json`. A limpeza da área de transferência é uma medida de melhor esforço:
substituir o conteúdo protege o novo valor, enquanto gerenciadores da área de transferência,
suspensão ou encerramento do processo auxiliar podem manter uma cópia.

Até aqui, tudo fica na sua máquina. Para sincronizar outro dispositivo, trabalhar em equipe ou usar
CI, faça login em um servidor:

```sh
sotto login                  # Sotto Cloud by default; --server <url> for your own server
sotto push                   # upload the active environment's ciphertext
sotto share DATABASE_URL     # a one-time link whose key never reaches the server
```

`sotto login` abre um navegador para entrar com o GitHub. Ele usa o Sotto Cloud em
[getsotto.co.uk](https://getsotto.co.uk), a menos que você passe `--server <url>` (consulte
[Auto-hospedagem](#auto-hospedagem) para hospedar o seu), e os comandos seguintes reutilizam o
servidor escolhido. De todo modo, o servidor só armazena texto cifrado: o cofre web em `/app` no
mesmo servidor descriptografa no seu navegador, com chaves que nunca saem dos seus dispositivos.

Para explorar os segredos interativamente, consulte o [guia do painel](#painel-interativo).

## Fluxos de trabalho com agentes

Agentes de programação locais podem descobrir e executar tarefas declaradas pelo projeto usando o servidor MCP local do Sotto. Consulte o [guia de tarefas para agentes](AGENT-WORKFLOWS.md) para configuração, aprovações e limites de segurança.

## Usando o Sotto

### Outro dispositivo

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Você precisa do Emergency Kit exibido por `sotto init`; sem ele, um novo dispositivo não consegue descriptografar o cofre.

Se você perder o Emergency Kit, `sotto reset` dá chaves novas à sua conta. É uma redefinição, não
uma recuperação: tudo o que foi criptografado com as chaves antigas fica ilegível para sempre, e os
administradores da organização precisam conceder de novo os ambientes compartilhados. Se algum
dispositivo ainda consegue desbloquear o cofre, exporte antes o que você precisa.

### Ambientes

Use `--env` para selecionar um ambiente para um único comando sem alterar o padrão do projeto:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` vale apenas para esse comando; `sotto env use` altera o ambiente padrão.

Liste os ambientes, altere o padrão, compare dois ou visualize uma cópia entre eles:

```sh
sotto env ls                          # list environments; the active one is marked
sotto env use staging                 # switch the project's active environment
sotto env diff dev staging            # compare keys; values stay hidden without --reveal
sotto env copy dev staging            # preview additions and updates without writing
sotto env copy dev staging --confirm  # apply the copy; never delete destination keys
```

`env diff` marca cada chave com `=` (mesmo valor), `!` (valores diferentes), `<` (só no primeiro
ambiente) ou `>` (só no segundo). Sem `--confirm`, a cópia é uma simulação. Uma cópia confirmada
adiciona e atualiza segredos, mas mantém intactas as chaves que existem apenas no destino.

### Importar um arquivo dotenv

`sotto import` lê um arquivo dotenv e define cada atribuição em um ambiente, criptografando cada valor
localmente. Ele grava no ambiente ativo, a menos que você indique outro com `--env`:

```sh
sotto import .env                         # into the active environment
sotto import --env staging .env.staging   # into staging; the default environment is unchanged
```

Uma importação é mesclada ao que já existe. Suponha que `staging` já tenha `API_URL` e `REGION`, e que
`.env.staging` contenha:

```dotenv
API_URL=https://staging.example.test
FEATURE_FLAG=on
```

Depois de importá-lo em `staging`:

- `API_URL` recebe o valor do arquivo, porque um nome existente é atualizado.
- `FEATURE_FLAG` é adicionado, porque um novo nome é criado.
- `REGION` é mantido, porque um nome ausente do arquivo nunca é removido.

A importação grava apenas no seu armazenamento local. Depois de `sotto login`, envie o resultado em uma etapa separada:

```sh
sotto push --env staging
```

O arquivo dotenv é apenas lido: a importação não o altera, não o consome nem o apaga, então proteja-o ou
apague-o você mesmo se ele tiver valores reais. Um arquivo que não pode ser analisado não altera nada, mas
uma falha durante a gravação não desfaz as atribuições já armazenadas; por isso, confira
`sotto ls --env staging` após uma importação interrompida.

Para recuperar um valor anterior, veja [Restaurar uma versão anterior de um segredo](#restaurar-uma-versão-anterior-de-um-segredo).
Esse histórico vem do servidor, então só ajuda com valores que você já havia enviado. Execute
`sotto import --help` para ver as regras de aspas e de valores literais em vez de repeti-las aqui.

### Remover um segredo

Para remover um segredo obsoleto de `staging` sem alterar o ambiente padrão:

```sh
sotto --env staging rm DEMO_OBSOLETE_KEY
sotto --env staging push
sotto --env staging pull
```

`sotto rm` remove o segredo do ambiente selecionado no cofre local.
`sotto push` sincroniza a remoção com o servidor depois de `sotto login`;
execute `sotto pull` no mesmo ambiente em outro dispositivo para receber
a remoção. Execute o último comando no outro dispositivo.

Em um terminal interativo, `sotto rm` sem nome abre o seletor de segredos.
Depois, aparece uma confirmação com Não como opção padrão; cancelar ou
recusar mantém o segredo. `--yes` (ou `-y`) ignora a confirmação, mas não
ignora o seletor sem nome nem uma solicitação de desbloqueio necessária.

As solicitações interativas exigem que stdin, stdout e stderr sejam terminais,
que `TERM` não seja «dumb» e que `CI` esteja inativo. Em uso não interativo,
é obrigatório informar o nome; a remoção com nome não pede confirmação
mesmo sem `--yes`.

Segredos removidos deixam de aparecer nas leituras e listagens comuns, mas
o histórico de versões é mantido. Isso não é uma exclusão segura e não revoga
a credencial no provedor externo. Para recuperação, use `sotto history`
e `sotto rollback`.

### Compartilhar um segredo

`sotto share` criptografa um segredo em um link de uso único. A chave fica depois do `#` no link,
uma parte que os navegadores nunca enviam a um servidor, então o servidor só guarda texto cifrado.
Criar um link exige `sotto login`; quem recebe não precisa de conta. Abrir o link não consome uma
visualização; revelar o segredo, sim.

```sh
sotto share DATABASE_URL                           # choose view and lifetime limits in an interactive terminal
sotto share DATABASE_URL --views 3 --expire 3600   # skip both limit prompts
sotto share DATABASE_URL --passphrase              # also require a passphrase; send it separately
```

Em um terminal interativo, cada limite omitido abre seu próprio menu. O menu de visualizações
oferece 1, 2, 3, 5 ou 10 visualizações e um valor personalizado; o menu de duração oferece sem
expiração, 1 hora, 1 dia, 7 dias, 30 dias ou um número de segundos personalizado. Os valores
personalizados devem ser de 1 a 100 visualizações ou de 1 a 2592000 segundos. Fornecer
`DATABASE_URL` ignora apenas a seleção do segredo: `--views` ignora o menu de visualizações e
`--expire` ignora o menu de duração de forma independente. Pressione Esc ou Ctrl-C em qualquer
menu de seleção para cancelar sem criar um compartilhamento.

O uso não interativo exige um nome de segredo explícito. Quando qualquer limite é omitido, seu
valor padrão é aplicado: uma visualização e sem expiração. Para ignorar os três menus de seleção
em um terminal interativo, forneça o nome e ambos os limites, por exemplo
`sotto share DATABASE_URL --views 3 --expire 3600`. As solicitações de desbloqueio e frase secreta
ainda podem ser exibidas.

Um servidor pode aplicar limites mais rígidos a algumas contas. Quando ele recusa uma solicitação, a
CLI mostra o motivo informado pelo servidor e não cria nenhum link.

Use `sotto share --no-copy` para desativar a cópia interativa, ou `--copy` para solicitá-la explicitamente.
Essas opções controlam apenas a área de transferência; `--no-copy` não ignora os menus de limites do compartilhamento.

Um link deixa de funcionar quando suas visualizações acabam, quando expira ou quando você o revoga.
Revogar impede que o servidor continue entregando o texto cifrado, mas não o apaga do servidor:

```sh
sotto shares ls              # your 100 most recent links: views used, expiry, revocation
sotto shares ls --json
sotto shares revoke <token>  # the token is the part of the link between /s/ and #
```

`sotto shares ls` nunca mostra o texto cifrado nem a chave do link.

Repare no plural: `sotto share` cria um link e `sotto shares` gerencia os links que você criou.
Ambos falam com o servidor configurado, então precisam de `sotto login`; o cofre local não precisa
estar desbloqueado. Uma listagem fica assim (token fictício, valores ilustrativos):

```text
active free links: 1
3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c  class=free  views=1/3  expires 2026-11-01 12:00:00+00
```

Passe a primeira coluna para `sotto shares revoke`. É o token público, não o link inteiro: para
`https://getsotto.co.uk/s/3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c#...`, passe só `3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c` e nunca nada depois
do `#`. A listagem lê apenas metadados. Ela não abre nenhum link nem consome uma visualização, e não
consegue reconstruir um link, porque a chave depois do `#` nunca chegou ao servidor.

`sotto shares ls --json` imprime um objeto, não um array simples: `active_free_count` e `links`, em
que cada link tem `token`, `share_class`, `max_views`, `view_count`, `expires_at`, `revoked_at` e
`created_at`. O servidor retorna só os seus 100 links mais recentes, então isso não é um histórico
completo.

Revogar impede novas recuperações pelo link. Não desfaz uma recuperação que já aconteceu; se o
destinatário pode ter revelado o segredo, troque-o na origem.

### Trabalhar em equipe

Equipes compartilham ambientes por meio de uma organização. Cada membro recebe sua própria cópia
selada da chave de um ambiente (uma concessão), e o servidor verifica papéis e concessões em cada
solicitação sem conseguir ler nada.

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto push                                 # create it on the server before sharing
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user; prints their user id
sotto grant <user-id>                      # share the active environment; prints their clone command
```

Depois, o membro executa o comando exibido. Isso não é o mesmo que **Outro dispositivo**, que restaura a sua própria conta em uma segunda
máquina. O membro usa a própria conta no mesmo servidor: executa `sotto login` e `sotto unlock` e
depois o comando exibido em um diretório que ainda não tem `sotto.toml`:

```sh
mkdir acme-api && cd acme-api
sotto clone <project-id> <env-id> --org <org-id>
```

Ser membro da organização e ter acesso a um ambiente são etapas separadas. `sotto org invite` torna
alguém membro; `sotto grant <user-id>` dá a essa pessoa um ambiente. Se você não tiver mais o ID do
usuário, `sotto org members <org-id>` mostra. `sotto clone` não concede nada sozinho: ele busca o
grant que o membro já tem, baixa esse ambiente e grava `sotto.toml`. Sem grant, ele para com
`you have not been granted this environment (ask an admin to share it)`, e um administrador precisa
executar `sotto grant` para essa pessoa. Nunca entregue o seu Emergency Kit a um colega; ele
desbloqueia a sua conta, não a dele.

Mantenha `--org` do comando exibido, porque ele registra a organização proprietária para que os
pushes seguintes correspondam ao servidor. `--as <environment-name>` e `--name <project-label>` só
definem rótulos locais: por padrão, o ambiente mantém o nome real e o projeto se chama `shared`.

Para ver quem tem acesso, remover alguém ou rotacionar você mesmo a chave de um ambiente:

```sh
sotto org members <org-id>                 # members, roles and user ids
sotto org remove <org-id> <user-id>        # remove a member and rotate every environment they held
sotto rotate                               # rotate the active environment's key
```

Remover um membro rotaciona a chave de todos os ambientes a que ele tinha acesso e revoga os tokens
de máquina que ele criou na organização, citando cada um para que você possa recriá-lo. A rotação
protege o que for escrito depois. Valores que o membro já viu continuam expostos, então troque-os na
origem.

#### Direitos antigos da organização

`sotto org plan <org-id>` lê a visão atual de direitos do servidor configurado para essa
organização. É preciso estar autenticado (`sotto login`); o cofre local não precisa estar
desbloqueado. Ele mostra os níveis atribuído e efetivo, a data de trial armazenada e os limites
aplicáveis, como `limits: none (Team)`. Esses campos mantêm a compatibilidade com registros e implantações antigas de planos
por organização; eles não descrevem os preços atuais do Cloud por pessoa nem uma nova oferta de
trial do Cloud.

Saída ilustrativa de um registro antigo de plano da organização:

```text
tier:      free
effective: free
trial:     ends 2026-07-16T09:00:00Z
limits:    3 members, 1 org project(s)
```

Um servidor auto-hospedado não aplica limites de plano, então sempre informa `team` como nível
efetivo, sejam quais forem o nível e o fim de trial que a organização tenha armazenados:

```text
tier:      free
effective: team
trial:     ends 2026-07-16T09:00:00Z
limits:    none (Team)
```

Esses exemplos mostram campos de compatibilidade armazenados, não as ofertas atuais do Cloud.
Implantações reais podem mostrar outros níveis, datas de trial e limites armazenados.

#### Log de auditoria da organização

Proprietários e administradores da organização podem consultar a atividade recente da equipe com:

```sh
sotto org audit <org-id>
sotto org audit <org-id> --limit 20
```

Use o ID exibido por `sotto org create <name>` ou listado por `sotto org ls`. Os resultados aparecem
do mais recente para o mais antigo; o padrão é de 100 eventos, e o servidor limita `--limit` ao
intervalo de 1 a 500. O comando mostra uma linha por evento no formato `timestamp  action  actor`,
com os campos opcionais `target=...`, `env=...` e `(detail)`. É uma visão limitada de atividade e
metadados, não uma forma de recuperar valores secretos; não há paginação nem um arquivo histórico
completo.

Este comando mantém a antiga verificação de acesso baseada no plano Team da organização.
Implantações auto-hospedadas ignoram essa verificação do plano, mas quem chama o comando ainda
precisa ter o papel de proprietário ou administrador. Se não houver eventos, o comando não imprime
linhas e termina com sucesso.

### Tokens de máquina e CI

Um token de máquina permite que a CI ou um serviço leia um ambiente sem senha nem Emergency Kit.
Com `SOTTO_TOKEN` definido, `sotto run` e `sotto export` buscam e descriptografam esse ambiente a
cada execução; defina também `SOTTO_SERVER`, como mostra o exemplo de [GitHub Actions](#github-actions).

Tokens de máquina expiram. Um token novo dura 90 dias, a menos que você passe
`--expires-in-days` com um valor de 1 a 365; `sotto token ls` mostra quando cada um termina.
Duas semanas antes, `sotto run` e `sotto export` exibem um aviso no log da CI.

Os comandos de token usam o acesso do usuário conectado e o ambiente selecionado. Em um ambiente
de organização, somente um administrador ou proprietário pode gerenciar tokens de máquina. A
lista contém apenas metadados de tokens ativos: tokens revogados ou expirados são omitidos, e
não é possível recuperar pela lista a credencial exibida uma única vez ao criar um token com
`sotto token create`. O JSON inclui `token_id`, `name`, `created_by`, `beneficiary_id`,
`beneficiary_status`, `expires_at` e `expires_in_days`; os campos de expiração podem ser null.
O `token_id` é o ID usado para revogar, não o nome do token nem o segredo `SOTTO_TOKEN`.

Use estes comandos para consultar, substituir e revogar um token. A seleção do ambiente vale
somente para este comando:

```sh
sotto token ls
sotto token ls --json
sotto --env staging token ls --json
sotto token create --name ci
sotto token revoke <token-id>
```

Para substituir um token de CI, identifique o ID antigo na lista, crie um substituto, atualize
o segredo da CI e confirme que ela já usa o novo token; depois revogue o ID antigo com
`sotto token revoke <token-id>`. A revogação impede imediatamente a autenticação desse token
na API, mas não rotaciona a chave do ambiente. Em um ambiente de organização, consulte as
orientações existentes de `sotto rotate` quando for necessário rotacionar a chave. Nem a
revogação nem a rotação apagam texto puro que já tenha sido copiado.

### Painel interativo

Em um projeto inicializado, execute `sotto` sem subcomando para explorar os segredos locais:

```sh
sotto
```

Você precisa ter uma identidade local desbloqueada antes de abrir o painel. Se necessário, Sotto
pede a senha mestra. A entrada, a saída e a saída de erro padrão precisam estar conectadas a
terminais. Se faltar algum, a CI estiver ativa ou `TERM=dumb`, a execução sem subcomando escreve
a ajuda na saída de erro padrão e termina com código 2. Use `sotto --help` para pedir a ajuda
explicitamente.

Use as setas ou `j` e `k` para navegar. Pressione `/` para buscar, `?` para ver a ajuda e Tab para
trocar de ambiente. A troca vale apenas durante a sessão do painel; `sotto env use` altera o
ambiente padrão do projeto. Pressione `r` para mostrar ou ocultar o valor selecionado, `c` para
copiá-lo para a área de transferência e `q` para sair. `Esc` fecha a busca ou a caixa de diálogo
atual; na tela principal, limpa uma busca ativa ou sai.

Pressione `t` para percorrer os temas com visualização ao vivo. Use as setas para ver cada tema
e pressione `Enter` para salvar sua escolha ou `Esc` para cancelar. As
[regras de precedência dos temas](#temas-de-saída) explicam como as opções do comando e as
variáveis de ambiente afetam a preferência salva.

### Consultar o estado local

Execute `sotto status` para ver se existe uma identidade local, se a sessão está desbloqueada e
qual projeto e ambiente o diretório atual seleciona. O comando lê o estado local. Ele não verifica
a conexão com o servidor nem prova que todos os segredos armazenados podem ser descriptografados.

```sh
sotto status
sotto status --json
```

Use `sotto status --json` em scripts. `initialized` indica se existe uma identidade local e
`unlocked` indica se uma chave mestra está em cache. `project` contém `name` e `environment` da
`sotto.toml` mais próxima ou é `null` fora de um projeto configurado. Um comando bem-sucedido
pode mostrar `unlocked: false`; scripts que precisam de uma sessão desbloqueada devem verificar esse campo.

Estes exemplos JSON são ilustrativos. Com uma `sotto.toml`, mas sem identidade local:

```json
{"initialized":false,"project":{"environment":"dev","name":"demo"},"unlocked":false}
```

Sem `sotto.toml` nem identidade local:

```json
{"initialized":false,"project":null,"unlocked":false}
```

Sem `sotto.toml`, nenhum projeto está selecionado. Um arquivo inválido ou ilegível causa um erro.

### Bloquear, desbloquear e sair

Com uma identidade local já inicializada, bloqueie o cofre, confira seu estado e
desbloqueie-o quando precisar usá-lo novamente:

```sh
sotto lock
sotto status
sotto unlock
```

`sotto lock` limpa a sessão local da chave mestra em cache usada pelos próximos
comandos da CLI. Não exclui sua identidade, o cofre criptografado nem o login salvo
do servidor de sincronização. `sotto unlock` usa sua senha mestra quando não há uma
sessão válida em cache. Os comandos que precisam descriptografar dados locais
também solicitam o desbloqueio quando necessário. Consulte
[Consultar o estado local](#consultar-o-estado-local) para conhecer os campos de estado.

A sessão local dura 12 horas a partir do armazenamento da chave em cache, não da
sua última atividade. A expiração é verificada quando a chave em cache é lida.
Executar `sotto unlock` quando o cofre já está desbloqueado não prolonga esse prazo.
Nem o bloqueio nem a expiração limpam o texto sem criptografia que os aplicativos
em execução já possuem.

Para limpar também o login da CLI salvo no servidor de sincronização, execute
os dois comandos:

```sh
sotto lock
sotto logout
```

`sotto logout` remove o token de sessão do servidor armazenado pela CLI,
independentemente do cache de desbloqueio local. Não encerra sessões do navegador
nem de outros dispositivos.

### Status do Cloud

Para consultar o servidor configurado e a elegibilidade do Cloud da sua conta, faça login primeiro:

```sh
sotto login
sotto cloud status
sotto cloud status --json
```

O comando exige uma sessão ativa, mas não desbloqueia o cofre local nem abre um navegador. Ele mostra o `server` configurado, o `server_info` do servidor, a `eligibility` da conta e um `billing_url` somente quando o estado retornado indica que a cobrança está disponível. A saída legível também mostra `actions`, `next_actions` e os valores opcionais `paid_through_epoch`, `recovery_until_epoch` ou `export_until_epoch`. A ausência de `server_info` ou `eligibility` significa que um servidor antigo não oferece esse endpoint; isso não prova que o acesso esteja permitido ou negado.

O formato JSON é útil para scripts. Sua estrutura de nível superior é:

```json
{
  "server": "https://getsotto.co.uk",
  "billing_url": "https://getsotto.co.uk/cloud",
  "server_info": {
    "deployment_mode": "cloud",
    "entitlement_model": "organisation_tiers_v1"
  },
  "eligibility": {
    "model": "person_eligibility_v1",
    "state": "paid",
    "deployment_mode": "cloud",
    "account_initialized": true,
    "billing_available": true,
    "paid_through_epoch": 1798761600,
    "recovery_until_epoch": null,
    "export_until_epoch": null,
    "actions": {
      "setup": false,
      "billing": false,
      "export": false,
      "revoke": true
    },
    "next_actions": ["revoke"],
    "payer": "personal"
  }
}
```

O exemplo é sintético; `actions` e as datas opcionais vêm do estado retornado da conta, e servidores antigos podem retornar null para qualquer um dos dois endpoints. `sotto cloud status` apenas informa o status e pode mostrar uma URL de cobrança. Ele não inicia uma compra nem altera a elegibilidade. Para diagnósticos do cofre local e dos ambientes, use `sotto status`; essa visão é separada das informações do servidor e da conta acima.

### Temas de saída

A CLI traz cinco temas integrados: `nord` (o padrão), `sordino`, `terminal`, `monochrome` e
`tokyo-night`. Gerencie-os com `sotto theme`:

```sh
sotto theme ls        # list available themes; the active one is marked
sotto theme set nord  # save a preference for later commands
sotto theme current   # print the theme this shell resolves to
```

`--theme <name>` escolhe um tema para um único comando. `SOTTO_THEME` o define para um shell, e
`sotto theme set` o salva. A precedência é `--theme`, depois `SOTTO_THEME`, depois a preferência
salva, depois `nord`; um nome desconhecido avisa na saída de erro padrão e volta para `nord`.

O estilo só se aplica a terminais interativos. `--plain`, um `NO_COLOR` não vazio (o contrato do
[no-color.org](https://no-color.org)), ambientes de CI, a saída padrão em um pipe e a entrada
padrão redirecionada suprimem cada um a cor e a decoração, mas mantêm a paleta escolhida, então
scripts e registros ficam em texto simples.

Temas personalizados são arquivos TOML no diretório `themes` dentro do diretório de dados da sua
plataforma: `~/Library/Application Support/sotto` no macOS, `%APPDATA%\sotto` no Windows e
`$XDG_DATA_HOME/sotto` ou `~/.local/share/sotto` no Linux (`SOTTO_DATA_DIR` muda esse
diretório). Cada arquivo `*.toml` precisa dos tokens abaixo e usa como nome o campo `name`, ou o
nome do arquivo quando esse campo não existe; um arquivo que não puder ser interpretado é
ignorado. As cores aceitam valores hexadecimais, nomes de cores ANSI, índices `ansi(<index>)` e
`default`.

```toml
bg = "#120024"
fg = "#ffffff"
accent = "#ff007f"
success = "#00ff66"
warning = "#ffaa00"
error = "#ff0033"
muted = "#775588"
border = "#331144"
```

Salvo como `synth.toml`, esse arquivo adiciona um tema `synth`. Aplique-o com
`sotto theme set synth`, ou para um único comando com `sotto --theme synth <command>`.

### Restaurar uma versão anterior de um segredo

Comece em um projeto já configurado e mantenha o mesmo ambiente selecionado durante todo o processo.
Você precisa estar conectado e com o armazenamento local desbloqueado. O histórico vem do servidor:
valores que nunca foram enviados não estão disponíveis nele para recuperação.

```sh
sotto history DATABASE_URL
```

Escolha um número de versão positivo e legível no histórico. O exemplo abaixo usa `2`;
substitua-o por uma versão disponível, sem o prefixo `v` exibido.

```sh
sotto rollback DATABASE_URL 2
sotto push
```

Rollback restaura o valor escolhido como uma nova versão local. Ele não apaga nem reescreve
o histórico anterior, e o servidor só é atualizado no envio seguinte.

Para obter JSON contendo apenas metadados:

```sh
sotto history DATABASE_URL --json
```

Cada linha contém `version`, `readable` (se foi possível descriptografar o valor) e `size_bytes`
(a quantidade de bytes do texto simples, ou null para uma versão ilegível). Nenhum valor é exibido.
Já `--reveal` exibe os valores secretos e não pode ser combinado com `--json`.

Se não houver histórico no servidor, confira o ambiente selecionado e se ele já foi enviado.
Enviar agora não recupera valores antigos que nunca foram sincronizados. Para uma versão
ilegível, siga a sugestão da CLI: execute `sotto pull` e tente novamente; isso não garante
que as chaves disponíveis permitam descriptografar essa versão.

Para scripts de shell e CI, consulte a [referência de scripts e códigos de saída da CLI](CLI-SCRIPTING.md).

### Variáveis de ambiente

A CLI lê estas variáveis:

- `SOTTO_SERVER`: a URL do servidor, que substitui a salva por `sotto login`. Com `SOTTO_TOKEN`, é
  obrigatória, a menos que `sotto login` tenha salvo um servidor nessa máquina.
- `SOTTO_TOKEN`: um token de máquina. `sotto run` e `sotto export` passam a ler o ambiente do token
  em vez do cofre local.
- `SOTTO_PASSWORD`: a senha mestra, para scripts que não conseguem responder a uma solicitação.
  Sempre que puder, prefira a solicitação oculta.
- `SOTTO_SECRET_KEY`: a Secret Key do seu Emergency Kit, para rodar `sotto setup` sem solicitação.
- `SOTTO_DATA_DIR`: muda o diretório de dados local, que guarda o armazenamento criptografado, as
  configurações salvas (como o servidor) e os temas personalizados.
- `SOTTO_THEME` e `NO_COLOR`: o estilo da saída; consulte [Temas de saída](#temas-de-saída).

## Auto-hospedagem

Um comando sobe uma implantação completa: Postgres, servidor e Caddy com HTTPS automático, a partir de
[`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); o passo a passo está em
[`deploy/README.md`](deploy/README.md). As partes também funcionam separadas: sirva o app web e a API na
**mesma origem** (para que o cookie de sessão e a CSP permaneçam na mesma origem); o [`Caddyfile`](Caddyfile)
incluído serve `web/dist` e faz proxy reverso da API, com cabeçalhos de segurança; o [`Dockerfile`](Dockerfile)
monta a imagem do servidor (as migrations rodam na inicialização).

`SOTTO_DEPLOYMENT_MODE` vale `self_hosted` por padrão. Nesse modo, o servidor não aplica limites
de plano nem outras restrições do serviço hospedado, e não precisa de uma conta Stripe. O modo
`cloud` é para o serviço que o Sotto opera; leia o passo a passo antes de defini-lo.

Aponte a CLI para o seu servidor uma vez; os comandos seguintes o reutilizam:

```sh
sotto login --server https://sotto.example.com
```

## Arquitetura

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

O workspace contém quatro crates:

- `crates/core`: os tipos criptográficos compartilhados e a implementação criptográfica completa.
- `crates/cli`: a interface de linha de comando `sotto` e o principal cliente nativo.
- `crates/server`: a API de sincronização baseada em Axum.
- `crates/wasm`: bindings `wasm-bindgen` que expõem o núcleo aos clientes web.

O cliente do navegador em `web/` é um app React e Vite que roda o núcleo por meio de `crates/wasm`.

## Pré-requisitos

- [Rustup](https://rustup.rs/) com Rust estável 1.89 ou mais recente
- Os componentes `clippy` e `rustfmt`
- O target `wasm32-unknown-unknown`
- Para o cliente web: Node.js 22 (o que o CI usa) e `wasm-pack` (o CI fixa a v0.15.0)
- Para o servidor e seus testes com banco de dados: Postgres 16, que `docker compose up -d` fornece
- Para as verificações de scripts: Python 3.11 ou mais recente

O `rust-toolchain.toml` incluído pede ao Rustup para instalar automaticamente os componentes e o target necessários.

## Desenvolvimento

Clone o repositório e compile e teste o workspace:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

`cargo test --workspace` não precisa de banco de dados, então ignora os testes de integração do
servidor e o teste de ponta a ponta da CLI sem falhar. Para executá-los, suba o Postgres local e
ative-os com `SOTTO_RUN_DB_TESTS=1`; eles recusam um `DATABASE_URL` que aponte para qualquer lugar
que não seja esta máquina:

```sh
docker compose up -d
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto SOTTO_RUN_DB_TESTS=1 \
  cargo test -p sotto-server -p sotto-cli
```

Use a CLI localmente (sem servidor):

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Os segredos ficam criptografados em repouso em um banco SQLite local; a chave mestra fica em cache no chaveiro do SO
com um TTL. Sincronizar com um servidor (`login`/`push`/`pull`/`setup`/`share`) é opcional.

### Executando o servidor

O servidor precisa de Postgres (um `docker compose up -d` sobe um para uso local):

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

O login com GitHub OAuth requer `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. Para qualquer deploy não local,
defina também `SOTTO_PUBLIC_URL` como a origem externamente acessível do servidor: ela monta a URL de callback do GitHub
e precisa ser igual ao callback registrado no app OAuth (o padrão é `http://localhost:8080`); e, para o cliente
web, `SOTTO_WEB_ORIGIN`. Sem OAuth o servidor continua subindo (serve `/health` e roda as migrations), mas o login
e todos os endpoints autenticados (sincronização, criação de compartilhamentos) ficam indisponíveis.

### Cliente web

O cliente do navegador roda o mesmo núcleo criptográfico via WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev        # dev server (proxies the API to localhost:8080)
npm run build      # production bundle → web/dist (strict CSP + Subresource Integrity)
npm run typecheck  # TypeScript checks; needs the WASM bindings a build generates
npm test           # unit tests (Vitest)
npm run smoke      # load the WASM core under Node and exercise it
```

A suíte Playwright do funil (`npm run e2e`) precisa de um banco de dados e de um build de produção
antes; [`web/e2e/README.md`](web/e2e/README.md) explica a preparação.

## Verificações de desenvolvimento

Rode as mesmas verificações básicas usadas pelo CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

A política de cadeia de suprimentos está definida em `deny.toml` e é verificada no CI com
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny):

```sh
cargo deny check
```

O job `supply-chain` do CI audita todo o lockfile com `scripts/check-cargo-audit`. O
[guia de auditoria do lockfile](.ci/cargo-audit.md) cobre a versão fixada do cargo-audit, os testes
da política e as regras das exceções adormecidas do lockfile.

A verificação cruzada entre implementações prova que os builds nativo e WASM concordam: texto cifrado produzido no nativo
é descriptografado byte a byte no WASM a partir de vetores de referência compartilhados:

```sh
wasm-pack test --node crates/wasm
```

O build web, sua auditoria de dependências e a verificação das traduções do README
(`scripts/check-readme-i18n`) também rodam no CI (`.github/workflows/ci.yml`).
[CONTRIBUTING.md](CONTRIBUTING.md) aponta para os guias de fuzzing, provas Kani, cobertura e
verificações de garantia.

## Telemetria

O **servidor** envia um ping anônimo por dia (nos primeiros 10-20 minutos após o boot) para
`https://getsotto.co.uk/telemetry/v1/ping`, para contar instâncias ativas e ver quais
versões estão por aí. A resposta nomeia a release mais recente, e o servidor registra uma linha quando está rodando
uma versão desatualizada. Este é o payload **inteiro**: o código de envio está em
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), e um teste unitário fixa o payload em exatamente
estes quatro campos:

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` é um UUID aleatório gerado uma vez e guardado no seu banco de dados, derivado de nada, então não identifica
hardware, host ou conta; apagá-lo transforma a instância em um contador anônimo novo. O lado da ingestão não guarda
endereços IP nem localização derivada. Sem contagens de orgs, membros ou segredos, e sem eventos de uso.
A **CLI, o cliente web e o WASM nunca enviam nada**.

Desative com `SOTTO_TELEMETRY=off` (ou o [`DO_NOT_TRACK=1`](https://consoledonottrack.com) válido para todas as
ferramentas): quando a telemetria está desativada, a tarefa nunca é iniciada e nenhuma requisição é feita. `SOTTO_TELEMETRY_URL` redireciona o ping
(por exemplo, para agregar uma frota privada), e registros que estão inativos há 12 meses são expurgados do censo hospedado.

## Segurança

O modelo do Sotto é de conhecimento zero: segredos em texto puro e chaves de descriptografia utilizáveis ficam nos
dispositivos cliente, e o servidor vê apenas texto cifrado mais metadados mínimos. Isso está implementado, mas **ainda não
auditado de forma independente**: consulte [SECURITY.md](SECURITY.md) para o modelo, a exposição honesta de metadados, como a
superfície web (baixada de novo a cada visita e, por isso, mais fraca) é reforçada e como verificar releases assinadas. O
modelo completo de adversários, garantias e não objetivos explícitos está publicado em [THREAT-MODEL.md](THREAT-MODEL.md).
Relate vulnerabilidades em privado conforme SECURITY.md.

O escopo e as evidências preparados para uma auditoria independente estão em [SECURITY-AUDIT-SCOPE.md](SECURITY-AUDIT-SCOPE.md).

## Contribuindo

O Sotto é Apache-2.0 e aceita contribuições. Comece por [CONTRIBUTING.md](CONTRIBUTING.md);
[good first issues](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
estão marcadas, e perguntas que não são bugs vão para
[Discussions](https://github.com/getsotto/sotto/discussions).

Relate vulnerabilidades em privado conforme [SECURITY.md](SECURITY.md).

## Licença

Licenciado sob a [Licença Apache, versão 2.0](LICENSE): todos os crates e o cliente web. Você só pode usar este projeto
em conformidade com a Licença. Exceto quando exigido pela lei aplicável ou acordado por escrito, o software distribuído
sob a Licença é distribuído "COMO ESTÁ", SEM GARANTIAS OU CONDIÇÕES DE QUALQUER TIPO.
