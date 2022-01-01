# rslide

`rslide` is a web service that allows you to move through multiple html pages in the browser like a slide, even without focusing on the app console or the browser. Currently only supports Windows.

The new page is sent via WebSocket. It's not for an advantage, but I
just want to try making something with WebSocket.

## Config

The app reads config from environment variable, with [`dotenv`](https://docs.rs/dotenv/0.15.0/dotenv/) supports.

| Variable | Description | Default Value | Required |
| -|-|-|-|
| LISTEN | Address the web service listens. The page can be viewed at `/`. | | ✔ | 
| TEMPLATE_PAGE | The path of the [template page](TODO) | | ✔ |
| PAGE_LIST | The page of the [page list file](TODO) | | ✔ |
| ASSETS_DIR | The page of the [assets directory](TODO) | | |
| HEARTBEAT | The duration of each heartbeat for keeping WebSocket alive. | 1s | |
| TIMEOUT | The duration of the timeout for waiting heartbeat response. | 10s | |
| PREV_CODE | The scancode of the key that switches to the previous page. | 26 (the `[` key) | |
| NEXT_CODE | The scancode of the key that switches to the next page. | 27 (the `]` key) | |

It uses [`humantime`](https://docs.rs/humantime/latest/humantime/index.html) to parse the duration like `HEARTBEAT` and `TIMEOUT` .

### Template Page

The template page is the skeleton of the pages. It should contains an element with id `mainHtml`, whose `innerHtml` will be set to the current page.

Template page doesn't support reloading. The change of the template page file won't take effect until you start a new service.

### Page List File

This file contains a list of the pathes of the `html` files of the pages, absolute or relative to this list file, line by line.

Pages listed in the file support reloading. The change of a page will take effect when refreshing or switch away and switch back to this page.

### Assets Directory

Optionally you can set a assets directory path, which will map to the `/assets` path in the web service.