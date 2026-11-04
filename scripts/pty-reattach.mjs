// Attach to a pty session twice and prove the scrollback survived the detach.
// Used by scripts/smoke.sh; Node is already a build dependency for the panel, so
// this avoids writing a WebSocket client in Rust just for the test.
//
// usage: node scripts/pty-reattach.mjs <hub> <pty_id>

const [, , hub, ptyId] = process.argv
if (!hub || !ptyId) {
  console.error('usage: pty-reattach.mjs <hub> <pty_id>')
  process.exit(2)
}
const wsBase = hub.replace(/^http/, 'ws')

function attach(send) {
  return new Promise((resolve) => {
    const ws = new WebSocket(`${wsBase}/api/pty?pty_id=${ptyId}`)
    let out = ''
    ws.onopen = () => {
      if (send) ws.send(JSON.stringify({ t: 'input', data: send }))
    }
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data)
      if (m.t === 'output') out += Buffer.from(m.data, 'base64').toString()
    }
    ws.onerror = () => resolve(out)
    setTimeout(() => {
      ws.close()
      resolve(out)
    }, 1500)
  })
}

const first = await attach('echo reattach-marker\n')
if (!first.includes('reattach-marker')) {
  console.error('first attach saw no output')
  process.exit(1)
}
const second = await attach(null)
if (!second.includes('reattach-marker')) {
  console.error('scrollback was not replayed after detach')
  process.exit(1)
}
console.log('  reattach ok')
