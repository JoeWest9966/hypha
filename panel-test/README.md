# panel test

Drives the real `index.html` + `js/app.js` inside jsdom with `fetch`,
`EventSource` and `WebSocket` stubbed to the hub's contract. Catches the whole
class of "the panel asked for a field the API doesn't send" bugs without a
browser.

```sh
cd panel-test
npm install
npm test
```
