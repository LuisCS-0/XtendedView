# Pantalla secundaria: prueba de concepto

Usa un móvil, una tablet u otro PC como pantalla del portátil desde el **navegador**, sin
instalar nada en el dispositivo. Este programa corre en el portátil y hace lo siguiente:

```
captura (Desktop Duplication) → H.264 (QuickSync) → WebSocket sobre HTTPS → navegador (WebCodecs) → canvas
                                       ↑                                                   │
                       ratón / táctil inyectados en Windows  ←──────────  táctil, ratón y lápiz
```

**Estado:** modo espejo (muestra la pantalla principal). El modo "extender" necesita un
monitor virtual y es la fase siguiente.

Hay dos versiones del programa del portátil, con el mismo protocolo y el mismo visor web:

| | Rust (`host-rs/`), recomendada | Python (`host/`), prototipo |
|---|---|---|
| Captura y codificación | Desktop Duplication → procesador de vídeo D3D11 → QuickSync (Media Foundation), todo en la GPU | FFmpeg 6.0 (`ddagrab` + `h264_qsv`), copias GPU→CPU→GPU |
| Cursor | aparte del vídeo (se mueve sin esperar a codificar) | dibujado en el vídeo |
| Keyframes | bajo demanda (recuperación inmediata tras pérdidas) | cada segundo |
| Dependencias | un ejecutable | Python + FFmpeg + OpenSSL |

## Host en Rust

```
cargo build --release --manifest-path host-rs\Cargo.toml
host-rs\target\release\pantalla-host.exe
```

Hay que lanzarlo desde la carpeta del proyecto, o indicarla con `--root`, para que encuentre `web\`
y `certs\`. Reutiliza el certificado y el código de emparejamiento de la versión Python, así que
los dispositivos ya configurados siguen funcionando. Opciones: `--monitor N`,
`--max-inflight-ms`, `--port`, `--http-port`, y `-v` para el diagnóstico del codificador.

Comparación en el mismo momento y con la misma sonda (visor en el propio portátil, 2048×1280 a
60 fps):

| Host | Latencia total | Captura → codificado |
|---|---|---|
| Python + FFmpeg | 83 ms (mín. 75, p95 95) | — |
| **Rust** | **70 ms (mín. 62, p95 73)** | 12,5 ms (7,9 ms a 120 fps) |

### Modo extender (pantalla nueva)

Con el modo extender, el dispositivo es un monitor más de Windows, no un espejo. Usa el driver
de monitor virtual de Parsec (Parsec VDD), firmado por Parsec:

1. Instálalo una vez, como administrador:
   https://builds.parsec.app/vdd/parsec-vdd-0.41.0.0.exe (la versión marcada como estable).
2. Arranca el host. El banner debe decir «Modo extender: disponible».
3. En el visor, elige **Modo → Extender**. El host conecta un monitor virtual con la resolución
   más parecida a la de la pantalla del dispositivo, lo captura y lo quita al desconectar. Si el
   host se cierra de golpe, el driver quita el monitor solo en un segundo.

Si ninguno de los modos de serie del driver coincide con tu pantalla (el Fold desplegado, por
ejemplo, es casi cuadrado), el visor muestra un aviso con la resolución exacta. Regístrala una vez,
desde PowerShell como administrador y con el visor desconectado:

```
tools\vdd-resoluciones.ps1 -Add 2184x1968@120
```

Admite hasta 5 resoluciones propias (`-List` las muestra y `-Clear` las borra). La escala del texto
del monitor virtual se ajusta en Configuración → Pantalla, y Windows la recuerda.

Pantallas en vertical (el Fold plegado, una tablet de pie): el driver solo ofrece modos
horizontales, así que el host elige el modo con las medidas intercambiadas y pone el monitor
virtual en orientación vertical, como un monitor girado. La imagen se endereza en la GPU al
convertirla, sin coste extra. Al girar o plegar el dispositivo, el monitor se adapta solo. Para
una resolución vertical exacta se registra con el lado largo primero (el aviso ya lo indica así).

### Lápiz (S Pen, Apple Pencil)

Con el host en Rust, el lápiz llega a Windows como un lápiz de verdad, no como un ratón:

- **Presión** (1024 niveles), **inclinación** y **giro**: los programas de dibujo y de notas
  varían el grosor del trazo igual que con un lápiz conectado al portátil.
- **Botón lateral** y **goma** (si el lápiz la tiene): Windows los trata como en un lápiz físico.
- **Lápiz flotando** sin tocar la pantalla: mueve el cursor, como el S Pen sobre el móvil.
- Se envían todas las muestras del lápiz, no solo una por fotograma, para que los trazos salgan
  suaves.

Si Windows no permite el lápiz sintético, el lápiz funciona como ratón.

Una lección importante: el codificador QuickSync de Intel espera internamente con el temporizador
de Windows, que por defecto avanza cada 15,6 ms. Por eso el host en Rust pide 1 ms
(`timeBeginPeriod`), desactiva el ahorro de energía del proceso y da prioridad MMCSS al hilo de
captura. Sin eso, cada frame tardaba unos 16 ms en codificarse; con ello, unos 4 ms.

## Requisitos de la versión Python (ya están en este portátil)

- Windows 10/11 y Python 3.11 o posterior (solo librería estándar).
- FFmpeg 6.0 o posterior con `ddagrab`. Por defecto se usa
  `C:\Software\ffmpeg-6.0-essentials_build\bin\ffmpeg.exe`; si está en otro sitio, usa `--ffmpeg`
  o la variable `FFMPEG`.
- `openssl` (para el certificado HTTPS local).

## Arrancar

```
python host\main.py
```

La consola muestra el **código de emparejamiento** y las direcciones a las que conectarse. La
primera vez se crea la carpeta `certs\`, con la autoridad de certificación propia, la clave del
servidor y el código. No la compartas.

## Probar con el Galaxy Fold (misma red Wi-Fi)

1. En Chrome, abre la dirección `https://192.168.0.15:8443/#k=…` que aparece en la consola. Si la
   IP cambia, la consola mostrará la nueva.
2. Chrome avisará del certificado: **Configuración avanzada → Acceder a … (sitio no seguro)**.
   Para no ver el aviso, instala el certificado desde `http://192.168.0.15:8080/setup`.
3. El código ya viene en la dirección. Pulsa **Conectar**.
4. Prueba a girar o desplegar el Fold: la imagen se adapta sola. El táctil del Fold funciona como
   táctil de Windows (desplazar, tocar, pellizcar).

Consejo: en el menú de Chrome, **Añadir a pantalla de inicio** la abre a pantalla completa como si
fuera una app.

### Medir la latencia total

1. En el portátil, abre `http://localhost:8080/probe` en cualquier ventana y déjala **visible**.
   No hace falta ponerla a pantalla completa.
2. En el visor, abre el menú (⋯) y marca **Medir latencia total**.
3. La línea `TOTAL` del panel de estadísticas mide desde que la sonda se dibuja en el portátil hasta
   que el móvil tiene el frame decodificado. La pantalla del móvil añade además alrededor de un
   refresco (8 ms a 120 Hz).

La sonda muestra también un reloj grande: una foto en la que salgan las dos pantallas sirve como
comprobación independiente.

| Línea del panel | Qué mide |
|---|---|
| Red | desde que el portátil envía el frame hasta que llega al móvil |
| Decodif. | tiempo de decodificación en el móvil |
| Portátil | frames codificados por segundo, frames descartados por congestión y variación del retardo del codificador |
| TOTAL | latencia de punta a punta (media, mínimo y percentil 95) |

## Sin router: red directa del portátil

Con el host en Rust:

```
host-rs\target\release\pantalla-host.exe --hotspot
```

Con la versión Python, en una consola aparte:

```
powershell -ExecutionPolicy Bypass -File tools\hotspot.ps1
```

Las dos formas crean una red Wi-Fi (WPA2) desde el portátil, sin necesidad de internet, y muestran
el nombre y la contraseña. Conecta el Fold a esa red y abre `https://192.168.137.1:8443/`. La red
existe mientras el host (o el script) esté abierto. El nombre y la contraseña se guardan en
`certs\hotspot.txt` para que el móvil la recuerde. El portátil puede seguir conectado a la Wi-Fi
de casa a la vez. Android avisará de que la red no tiene internet: elige mantener la conexión.

Si el portátil sigue conectado a otra Wi-Fi, la tarjeta tiene que repartirse entre las dos y puede
haber más variación de latencia. Para medir lo mejor posible, desconéctala.

## Opciones

| Opción | Por defecto | Para qué |
|---|---|---|
| `--encoder qsv\|x264` | `qsv` | QuickSync (hardware) o x264 (CPU) |
| `--pipeline cpu\|gpu` | `cpu` | dónde se escala la imagen antes de codificar (solo con QuickSync) |
| `--monitor N` | `0` | monitor a capturar |
| `--gop S` | `1.0` | segundos entre keyframes (tiempo máximo de recuperación tras una pérdida) |
| `--max-inflight-ms MS` | `200` | retraso acumulado a partir del cual se descartan frames en vez de acumular retraso |
| `--port`, `--http-port` | `8443`, `8080` | puertos HTTPS y HTTP |

Desde el visor se eligen los fotogramas por segundo (30–120), la calidad (10–50 Mbps) y la
resolución (100/75/50 %).

## Resultados en este portátil

Core Ultra 7 258V y Arc 140V. Visor en el propio portátil (sin Wi-Fi de por medio), 2048×1280 a
60 fps. La sonda se dibuja en Edge, que ya añade unos 15–25 ms por sí solo.

| Configuración | fps reales | Latencia total |
|---|---|---|
| QuickSync con escalado en GPU | ~35 | 76 ms (mín. 67, p95 85) |
| **QuickSync con escalado en CPU (por defecto)** | ~59 | **64 ms (mín. 55, p95 70)** |
| x264 (CPU) | ~55 | 60 ms (mín. 53, p95 65), usando más de un núcleo |

En las tres, la red quedó por debajo de 1 ms y la decodificación en torno a 0,7 ms. El límite está en
la captura: con FFmpeg 6.0 cada frame se copia de la GPU a la CPU y, con QuickSync, de vuelta a la
GPU. Una versión de FFmpeg con oneVPL, o una captura nativa, permitiría hacerlo todo en la GPU.

## Limitaciones actuales

- Modo extender: solo con el host en Rust y el driver Parsec VDD instalado.
- Con el host en Python, el lápiz funciona como ratón, sin presión.
- Sin audio.
- iPad: el mismo visor debería funcionar en Safari con iPadOS 16.4 o posterior. Ahí es mejor
  **instalar el certificado** desde `/setup` que aceptar el aviso.
- Emparejamiento: por ahora, escribiendo la dirección. QR y Bluetooth vendrán después.

## Estructura

```
host-rs/src/       host en Rust: capture.rs (DDA y cursor), convert.rs (BGRA→NV12 en GPU),
                   mfenc.rs (QuickSync), pipeline.rs (hilo de captura), server.rs (HTTPS/WS),
                   vdd.rs + display.rs (monitor virtual), certs.rs, input.rs (táctil/ratón), h264.rs
tools/vdd-resoluciones.ps1  resoluciones propias del monitor virtual (administrador)
host/main.py       servidor HTTPS/WebSocket, sesión del visor, control de flujo y estadísticas
host/encoder.py    comando de FFmpeg y lectura de la salida FLV frame a frame
host/websocket.py  WebSocket mínimo (RFC 6455)
host/certs.py      autoridad de certificación local y certificado con las IP del portátil
host/winapi.py     DPI, monitores e inyección de táctil/ratón (ctypes)
web/               visor (index.html, viewer.js), sonda (probe.*), configuración (setup.html)
tools/hotspot.ps1  red Wi-Fi directa del portátil (Wi-Fi Direct en modo legacy)
```
