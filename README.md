# ticktick-tui

Um gerenciador de tarefas minimalista em Rust para o terminal, conectado à Open API do TickTick. Navegação inspirada no Vim, projetos à esquerda, tarefas no centro e detalhes à direita em terminais largos.

## Instalação

```sh
cargo install --path . --locked
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
| `e` | Editar título e conteúdo no editor |
| `x`, `Space` | Concluir tarefa |
| `dd` | Excluir, com confirmação `y` |
| `p` | Alternar prioridade: nenhuma → baixa → média → alta |
| `/` | Filtrar títulos; Enter aplica |
| `Esc` | Cancelar entrada ou limpar filtro |
| `Enter` | Mostrar / ocultar detalhes |
| `r`, `:sync`, `:w` | Sincronizar |
| `:complete`, `:delete` | Concluir / excluir |
| `:noh` | Limpar filtro |
| `:config` | Mostrar caminho do arquivo de configuração |
| `:token` | Configurar ou trocar o API token no TUI |
| `?`, `:help` | Ajuda |
| `q`, `:q`, `Ctrl-c` | Sair |

Selecione uma lista específica antes de criar tarefas: a visão “All tasks” não determina um projeto de destino. No editor, a primeira linha é o título e o restante é o conteúdo. `$VISUAL` tem precedência sobre `$EDITOR`; o padrão é `vi`. Use o caminho de um executável ou script wrapper, sem argumentos na variável.

A interface usa modos Normal, Inserção e Comando. Não implementa todo o motor de edição do Vim (macros, registros e operadores de texto); para todos os comandos de edição, use `e` com Vim/Neovim.

## Cores nos dotfiles

```sh
mkdir -p ~/.config/ticktick-tui
ticktick-tui config > ~/.config/ticktick-tui/config.toml
```

Pode versionar `config.toml` nos dotfiles e criar um symlink. Também são respeitados `XDG_CONFIG_HOME` e `XDG_CACHE_HOME`. Exemplos completos em `themes/`; copie o tema escolhido para `config.toml`. Reinicie para aplicar alterações.

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
