import path from 'node:path'

const port = Number.parseInt(process.env.PORT ?? '80', 10)
const host = process.env.HOST ?? '0.0.0.0'
const distDir = new URL('./dist/', import.meta.url)

const contentTypes = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.mjs', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.svg', 'image/svg+xml'],
  ['.png', 'image/png'],
  ['.jpg', 'image/jpeg'],
  ['.jpeg', 'image/jpeg'],
  ['.ico', 'image/x-icon'],
  ['.woff', 'font/woff'],
  ['.woff2', 'font/woff2'],
  ['.txt', 'text/plain; charset=utf-8'],
])

async function serveFile(fileUrl, pathname) {
  const file = Bun.file(fileUrl)

  if (!(await file.exists())) {
    return null
  }

  const extension = path.extname(pathname).toLowerCase()
  return new Response(file, {
    headers: {
      'Content-Type': contentTypes.get(extension) ?? 'application/octet-stream',
      'Cache-Control': extension === '.html' ? 'no-cache' : 'public, max-age=31536000, immutable',
    },
  })
}

globalThis.frontendServer = Bun.serve({
  port,
  hostname: host,
  async fetch(request) {
    const requestUrl = new URL(request.url)
    const pathname = requestUrl.pathname === '/' ? '/index.html' : requestUrl.pathname
    const relativePath = pathname.startsWith('/') ? pathname.slice(1) : pathname
    const staticResponse = await serveFile(new URL(relativePath, distDir), pathname)

    if (staticResponse) {
      return staticResponse
    }

    if (!path.extname(pathname)) {
      const fallbackResponse = await serveFile(new URL('index.html', distDir), '/index.html')

      if (fallbackResponse) {
        return fallbackResponse
      }
    }

    return new Response('Not Found', {
      status: 404,
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
    })
  },
})

globalThis.frontendServer.ref()

console.log(`Frontend server listening on http://${host}:${port}`)