import { defineConfig, type ProxyOptions } from 'vite'
import react from '@vitejs/plugin-react'

// Tipo estrutural mínimo da instância de proxy que o Vite passa a configure().
// O Vite 8 não depende mais do pacote `http-proxy` (e não há @types/http-proxy),
// então captamos apenas o método `emit` que usamos. A atribuição a `configure`
// é feita com cast por isso.
interface ProxyServer {
  emit(event: string, ...args: unknown[]): boolean
}

// Silencia erros transientes de proxy (EPIPE, ECONNRESET) que ocorrem quando
// o backend reinicia ou o client desconecta abruptamente dentro do Docker.
//
// O Vite chama configure() ANTES de registrar seus próprios listeners, então
// removeAllListeners não funciona. A solução é interceptar no nível do emit()
// para suprimir erros antes que qualquer handler (inclusive o do Vite) os veja.
function silenceProxyErrors(proxy: ProxyServer) {
  const originalEmit = proxy.emit.bind(proxy)

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  proxy.emit = function (event: string, ...args: any[]): boolean {
    if (event === 'error') {
      const err = args[0] as NodeJS.ErrnoException | undefined
      if (err?.code === 'EPIPE' || err?.code === 'ECONNRESET' || err?.code === 'ECONNREFUSED') {
        // Fecha a resposta silenciosamente se ainda estiver aberta
        const res = args[2]
        if (res && typeof res === 'object' && 'writeHead' in res && !(res as import('node:http').ServerResponse).headersSent) {
          (res as import('node:http').ServerResponse).writeHead(502).end()
        }
        return true // finge que o evento foi tratado
      }
    }

    // Intercepta proxyReqWs para silenciar erros EPIPE no socket downstream
    if (event === 'proxyReqWs') {
      const socket = args[2] as import('node:net').Socket | undefined
      if (socket && typeof socket.emit === 'function') {
        const origSocketEmit = socket.emit.bind(socket)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        socket.emit = function (ev: string, ...sArgs: any[]): boolean {
          if (ev === 'error') {
            const sErr = sArgs[0] as NodeJS.ErrnoException | undefined
            if (sErr?.code === 'EPIPE' || sErr?.code === 'ECONNRESET') {
              return true
            }
          }
          return origSocketEmit(ev, ...sArgs)
        } as typeof socket.emit
      }
    }

    return originalEmit(event, ...args)
  } as typeof proxy.emit
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': {
        target: process.env.VITE_PROXY_TARGET ?? 'http://localhost:8000',
        changeOrigin: true,
        configure: silenceProxyErrors as ProxyOptions['configure'],
      },
      '/ws': {
        target: process.env.VITE_PROXY_WS_TARGET ?? process.env.VITE_PROXY_TARGET ?? 'ws://localhost:8000',
        ws: true,
        changeOrigin: true,
        configure: silenceProxyErrors as ProxyOptions['configure'],
      },
    },
    hmr: {
      // Caminho explícito para o HMR do Vite, evitando colisão com o proxy /ws
      path: '/__vite_hmr',
    },
    watch: {
      usePolling: true, // Necessário para Windows + Docker Volumes funcionarem bem
    }
  }
})
