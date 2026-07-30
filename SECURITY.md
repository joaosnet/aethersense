# Segurança

Não publique vulnerabilidades, tokens, senhas, nomes de rede, MACs,
coordenadas ou capturas residenciais em *issues*.

Para relatar uma vulnerabilidade, use o recurso **Report a vulnerability** na
aba **Security** do repositório GitHub. Inclua uma descrição, impacto, versão
afetada e passos mínimos de reprodução sem dados pessoais.

## Segredos

O AetherSense recebe credenciais por variáveis de ambiente ou pelo mecanismo
local de provisionamento. Nunca versione:

- `.env`;
- `aethersense_config.json`;
- `sdkconfig` com SSID ou senha;
- conteúdo de `mosquitto/runtime/`;
- capturas CSI ou sessões contendo identificadores reais.

Se um segredo for publicado, revogue-o imediatamente antes de remover o
arquivo do histórico.
