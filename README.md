# ticktick-tui

Um gerenciador de tarefas minimalista em Rust para o terminal, conectado à Open API do TickTick. Navegação inspirada no Vim, projetos à esquerda, tarefas no centro e detalhes à direita em terminais largos.

## Instalação

Requer Rust (https://rustup.rs). Não precisa clonar o repositório:

```sh
cargo install --git https://github.com/amonteirom96/ticktick-tui --locked
# Para atualizar, rode o mesmo comando com --force.
# Explore a interface sem conta e sem chamadas de rede:
ticktick-tui demo
```

## Conectar sua conta

Abra o aplicativo:

```sh
cargo run
```

Sem credenciais salvas, o próprio TUI abre a tela **API token**. Gere um API Token no TickTick, cole na tela e pressione `Enter`. O aplicativo valida o token antes de salvar e inicia a sincronização. A entrada fica oculta; `Ctrl-u` limpa o campo e `Esc` cancela. Não é necessário configurar variáveis de ambiente ou um app OAuth para usar um API token.

Para trocar o token depois, digite `:token` e pressione `Enter`. A troca limpa o snapshot local da conta anterior e carrega as tarefas da nova conta. Durante uma sincronização, aguarde a conclusão antes de trocar. Cancelar ou falhar na validação mantém a credencial anterior. O modo `demo` continua sem configurar conta ou fazer chamadas de rede.

O token não faz parte do arquivo de tema. Também é possível fornecer `TICKTICK_TOKEN` como alternativa quando não existir token salvo.

OAuth:

1. Crie um app em https://developer.ticktick.com/manage.
2. Cadastre exatamente `http://localhost:8080/callback` como Redirect URI.
3. Configure as credenciais e autentique:

```sh
export TICKTICK_CLIENT_ID='seu-client-id'
export TICKTICK_CLIENT_SECRET='seu-client-secret'
ticktick-tui auth
ticktick-tui
```

O comando exibe um link. Abra no navegador, autorize e cole a URL completa do redirecionamento no terminal. A página localhost pode falhar ao carregar: o aplicativo lê a URL colada, sem iniciar um servidor HTTP. O parâmetro OAuth `state` e a URL de retorno são validados. Para outro endereço, defina `TICKTICK_REDIRECT_URI` com o mesmo valor cadastrado no portal.

O token é salvo separadamente em `~/.config/ticktick-tui/token` (permissão 0600 em Unix). Não versione esse arquivo ou credenciais. O token salvo tem precedência sobre `TICKTICK_TOKEN`. Para remover a autenticação local, remova o arquivo e a variável; isso não revoga acesso no servidor. Quando expirar, execute `auth` novamente.

## Teclas

| Tecla | Ação |
| --- | --- |
| `h` / `l`, `Tab` | Focar listas / tarefas |
| `j` / `k`, `5j` / `5k` | Navegar, com contagem opcional |
| `gg` / `G` | Primeiro / último item |
| `Ctrl-d` / `Ctrl-u` | Descer / subir 10 itens |
| `a`, `i`, `o` | Criar tarefa na lista selecionada |
| `cc` | Renomear tarefa |
| `v` + `j`/`k`, `J` / `K` | Selecionar e mover tarefas na lista, com contagem (`3J`); `v`/`Esc` sai. Ordem manual (`sortOrder`) do TickTick; subtarefas acompanham o pai |
| `e` | Editar título e conteúdo no editor |
| `x`, `Space` | Concluir tarefa |
| `dd` | Excluir, com confirmação `y` |
| `p` | Alternar prioridade: nenhuma → baixa → média → alta |
| `a` / `cc` / `dd` na preview | Novo item / renomear / apagar item do checklist (Enter adiciona o próximo) |
| `m` | Mover tarefa para outra lista (digite parte do nome) |
| `D` | Prazo: escolha Hoje / Amanhã / Próxima semana / Sem prazo, ou digite `sex 14:30`, `+3`, `25/12` |
| `R` | Lembretes: `Space` marca as opções, ou digite `2h`, `3d` |
| `za` / `zo` / `zc` | Recolher / expandir pasta (em Listas) ou subtarefas |
| `/` | Filtrar títulos; Enter aplica |
| `Esc` | Cancelar entrada ou limpar filtro |
| `Enter` | Mostrar / ocultar detalhes |
| `zp` | Mostrar / ocultar preview |
| `f` | Zoom: preview em tela cheia (`f`/`Esc` volta) |
| `<` / `>` | Estreitar / alargar a coluna de tarefas |
| `b` | Ocultar / mostrar a coluna de listas (`h` reabre) |
| `r`, `:sync`, `:w` | Sincronizar |
| `:complete`, `:delete` | Concluir / excluir |
| `:noh` | Limpar filtro |
| `:config` | Mostrar caminho do arquivo de configuração |
| `:token` | Configurar ou trocar o API token no TUI |
| `?`, `:help` | Ajuda |
| `q`, `:q`, `Ctrl-c` | Sair |

Selecione uma lista específica antes de criar tarefas: a visão “All tasks” não determina um projeto de destino. No editor, a primeira linha é o título e o restante é o conteúdo. `$VISUAL` tem precedência sobre `$EDITOR`; o padrão é `vi` em Unix e `notepad.exe` em Windows. Use o caminho de um executável ou script wrapper, sem argumentos na variável.

A interface usa modos Normal, Inserção e Comando. Não implementa todo o motor de edição do Vim (macros, registros e operadores de texto); para todos os comandos de edição, use `e` com Vim/Neovim.

## Cores nos dotfiles

```sh
mkdir -p ~/.config/ticktick-tui
ticktick-tui config > ~/.config/ticktick-tui/config.toml
```

Pode versionar `config.toml` nos dotfiles e criar um symlink. Também são respeitados `XDG_CONFIG_HOME` e `XDG_CACHE_HOME`. Exemplos completos em `themes/`; copie o tema escolhido para `config.toml`. Reinicie para aplicar alterações.

Sem `[theme]` nem `theme_file` no `config.toml`, o tema segue o fundo do terminal: claro em terminais claros, escuro nos demais. O `config.toml` gerado pelo comando acima já inclui `[theme]`; remova essa seção para voltar à detecção automática.

```toml
sync_seconds = 60

[theme]
background = "#161b22"
foreground = "#c9d1d9"
muted = "#8b949e"
accent = "#79c0ff"
selection = "#263449"
danger = "#ff7b72"
success = "#7ee787"
```

## Sincronização

Leitura inicial e atualizações periódicas em uma thread separada, sem bloquear a navegação. A frequência mínima é 10 segundos. Alterações são enviadas imediatamente e seguidas de uma leitura completa dos projetos abertos. Campos adicionais das tarefas, como checklist e recorrência, são preservados ao editar.

O último snapshot completo é salvo atomicamente em `~/.cache/ticktick-tui/cache.json`, com permissão restrita em Unix. Sem rede, os dados permanecem disponíveis para consulta. Não há fila de escritas offline ou resolução de conflitos: outra alteração feita no mesmo momento em outro cliente pode prevalecer ou ser sobrescrita. Falhas de escrita não geram tentativas automáticas, evitando duplicações; atualize antes de tentar novamente quando o resultado for incerto. Excluir é permanente pela API. A API é consultada projeto a projeto; uma conta com muitas listas pode levar mais tempo para atualizar.

Este cliente cobre tarefas dos projetos retornados pela API pública; não presume paridade com todos os recursos do site, como hábitos ou histórico de tarefas concluídas. Datas e descrições aparecem nos detalhes; a interface atual edita título, conteúdo e prioridade. A autenticação e as chamadas reais exigem credenciais do usuário.

Referência usada na implementação: [documentação oficial TickTick Open API](https://developer.ticktick.com/docs/index.html#/openapi) ([Markdown](https://developer.ticktick.com/docs/openapi.md)).

## Desenvolvimento

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- demo
```

## Downloads e releases

A pipeline `.github/workflows/release.yml` testa e gera seis binários: Linux x64 e ARM64, macOS Intel e Apple Silicon, Windows x64 e ARM64. Arquivos `.tar.gz` (Unix) e `.zip` (Windows) incluem o executável, licença, documentação e temas. As releases incluem `SHA256SUMS`.

Depois que o workflow estiver no GitHub, ajuste `version` no `Cargo.toml`, atualize o `Cargo.lock` e envie uma tag correspondente, por exemplo `v0.1.0`. A tag deve corresponder à versão do pacote. Todos os builds precisam passar antes da publicação automática em [Releases](https://github.com/amonteirom96/ticktick-tui/releases). A execução manual em Actions gera artefatos para testar, sem publicar release.

Extraia o download correspondente ao seu sistema e arquitetura e coloque `ticktick-tui` (ou `ticktick-tui.exe`) no PATH. Não é necessário instalar Rust. Os binários Linux requerem glibc: 2.35+ em x64 e 2.39+ em ARM64; Alpine/musl não é suportado nesses pacotes. Os binários macOS e Windows não são assinados.

## Today e relatório diário

O aplicativo abre com foco na primeira coluna e **Today** selecionado. A ordem é Today, Inbox, Amanhã, Relatório do dia, Hábitos, Pomodoro, Stopwatch, suas listas e Todas as tarefas. Today inclui tarefas atrasadas; Amanhã considera somente a data seguinte no fuso do TickTick (ou do sistema quando indisponível). Nessas duas visões, cada tarefa mostra discretamente, à direita, o nome da lista a que pertence.

O **Relatório do dia** começa na data atual e exibe cartões com contadores grandes, distribuição visual por lista e uma tabela compacta de conquistas. O título completo da conquista selecionada aparece abaixo da tabela. Estados vazios ficam centralizados; ao consultar uma data, um indicador animado aparece no centro. Use `[` para o dia anterior, `]` para o seguinte, `t` para hoje e `:report AAAA-MM-DD` para uma data específica. `j`/`k` selecionam conquistas quando a coluna central está focada. `l` ou `Enter` abre a atividade concluída com descrição e checklist; `j`/`k` rolam o preview e `h`/`Esc` volta. O resumo separa as quantidades por prioridade, e os números por lista aparecem sobre as barras com texto na cor do painel.

O cliente tenta consultar `/task/completed` para a data escolhida. Esse endpoint e `/user/info` são opcionais e não têm disponibilidade garantida na Open API. Quando o histórico não está disponível ou a resposta está truncada, o relatório avisa e apresenta as conclusões confirmadas neste aplicativo, preservadas no cache. Excluir uma tarefa não conta como conclusão. Não é possível garantir a recuperação de conclusões antigas ou realizadas em outros clientes com todos os tipos de token. O relatório é uma visão local, sem criar uma lista no TickTick.

O cabeçalho mostra o nome recebido do perfil; a sincronização aparece no rodapé. Se o token não permitir ler o perfil, defina `user_name = "Seu nome"` no início de `~/.config/ticktick-tui/config.toml`, antes de `[theme]`; até então será exibido “Minha conta”.

Os temas incluídos usam uma superfície uniforme entre fundo e painéis, com bordas discretas e destaque na seleção. Configurações de cores personalizadas continuam tendo precedência.

O painel **Tempo focado por lista** soma registros reais de Pomodoro e cronômetro na data escolhida, mesmo que as tarefas ainda não tenham sido concluídas. Mostra horas e minutos (por exemplo, `Expin · 2h 30min`). Sessões sem tarefa aparecem como “Foco livre”; tarefas cujo projeto não pôde ser identificado aparecem separadamente. Se a API falhar, o relatório informa a indisponibilidade em vez de apresentar zero. Os registros da API são convertidos de milissegundos; sessões que atravessam datas ou tarefas têm a duração efetiva distribuída proporcionalmente pelos intervalos.

## Hábitos, Pomodoro e Stopwatch

Logo abaixo do relatório, **Hábitos** consulta `/habit` e mostra os nomes e metas do TickTick. **Pomodoro** e **Stopwatch** consultam `/focus` para mostrar as sessões de hoje por tipo. Se o token não permitir um endpoint, a tela informa a indisponibilidade; `r` tenta novamente.

Hábitos mostra apenas os programados para hoje, seguindo a `repeatRule` do TickTick (dias da semana em `BYDAY`, ou `INTERVAL` diário a partir da data de início); hábitos arquivados ou que ainda não começaram ficam ocultos. `Space` ou `x` conclui o hábito selecionado; num hábito já concluído, desmarca (envia o check-in do dia com valor 0).

Na coluna central de Pomodoro e Stopwatch, `Space` ou `Enter` inicia/pausa o timer; `s` zera; `j`/`k` rola. `t` escolhe a atividade (busca entre as tarefas abertas, com as de hoje/atrasadas primeiro; a primeira opção é “Foco livre”). `c` define os tempos de foco, pausa e pausa longa em minutos (ex.: `50 10 20`; padrão `25 5 15`). Ao fim de um foco, `Space` inicia a pausa, e ao fim da pausa volta ao foco; a cada 4 focos concluídos a pausa é longa. `n` pula para a próxima fase. Há um timer local por vez. O estado (fase, tempos e atividade) é salvo em `focus-timer.json` junto ao cache e continua ao fechar e reabrir o TUI; o modo demo não persiste o timer. Não há alarme com o aplicativo fechado. Estes timers não iniciam foco nos outros dispositivos e não enviam sessões ao histórico do TickTick.
