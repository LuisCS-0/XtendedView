# XtendedView

Usa un móvil, una tablet u otro PC como pantalla del portátil desde el **navegador**, sin
instalar nada en el dispositivo. Este programa corre en el portátil y hace lo siguiente:

```
captura (Desktop Duplication) → H.264 (QuickSync) → WebSocket sobre HTTPS → navegador (WebCodecs) → canvas
                                       ↑                                                   │
                       ratón / táctil inyectados en Windows  ←──────────  táctil, ratón y lápiz
```

## Uso

1. Arranca `host-rs\target\release\xtendedview.exe` desde la carpeta del proyecto. Se abre el
   **panel de control** en una ventana (también en `http://localhost:8080/panel`).
2. Elige cómo conectar: **Red del ordenador**, **Red propia** (el ordenador crea su Wi-Fi, sin
   router) o **Cable USB** (experimental). Al elegir, se enciende. Cada encendido crea un **código de
   emparejamiento nuevo**, así que hay que volver a escanear el QR. Mientras está apagada, ningún
   dispositivo puede conectarse, no hay red propia ni monitor virtual. El botón **Apagar** de arriba
   lo detiene todo.
3. En el móvil o la tablet, **escanea con la cámara el código QR** del panel. Con red propia hay
   dos: el primero une el dispositivo a la red y el segundo abre la pantalla.
4. **Acepta la conexión en el panel.** Cada dispositivo que llega aparece arriba con su nombre, su
   dirección y por dónde llega, con **Aceptar** y **Rechazar** (la ventana del panel parpadea en la
   barra de tareas). Si no se acepta en 90 s, se rechaza solo. Un dispositivo ya aceptado no vuelve a
   preguntar hasta que apagues (al reconectar tras un corte o al recargar la página).

### Varios dispositivos a la vez

Hasta 4, cada uno con sus propios ajustes, que se guardan para la próxima vez (el navegador de cada
dispositivo guarda un identificador):

- **Pantalla adicional o espejo**, cada uno por su lado: dos pantallas adicionales, una adicional y
  otra en espejo, dos en espejo del mismo monitor… Cada pantalla adicional tiene su monitor virtual,
  en el lado elegido; un dispositivo nuevo va a un lado libre, y dos en el mismo lado se colocan uno
  tras otro.
- **Fotogramas, calidad, resolución, pantalla completa y estadísticas** por dispositivo.
- **Por la conexión que quiera:** en la tarjeta «Conexiones» se activan por separado la red del
  ordenador, la red propia y el cable, y puede haber varias a la vez (por ejemplo, la tablet por la red
  propia y el móvil por cable). Solo se aceptan dispositivos por las conexiones activas.

Windows solo deja capturar cada monitor una vez por programa: si dos dispositivos duplican el mismo,
comparten la captura y cada uno recibe la imagen a su tamaño, fotogramas y calidad.

El dispositivo solo muestra la pantalla, sin menús. Todo se ajusta desde el panel del ordenador,
en la tarjeta de cada dispositivo: modo, lado o monitor, fotogramas, calidad, resolución, pantalla
completa, estadísticas, sus medidas (latencias, fotogramas, velocidad, codificador), registrar la
resolución exacta y desconectarlo. En la parte general: medir la latencia y cambiar el código.

Los ajustes se guardan en `certs\ajustes.json`. Al encender, la consola del host también muestra
el código QR. Con `--no-panel` no se abre la ventana del panel; con `--start` arranca ya
encendida; con `--hotspot` se elige la red propia.

El host no consulta el nombre de la Wi-Fi del ordenador: desde Windows 11 24H2 eso cuenta como usar
la ubicación, y aparecería el aviso en la barra de tareas. Para saber si el ordenador sigue
conectado a otra Wi-Fi, mira las tarjetas de red y sus direcciones.

## Versiones del programa del ordenador

Hay dos versiones, con el mismo protocolo y el mismo visor web. El panel, los QR y la pantalla
adicional solo están en la de Rust:

| | Rust (`host-rs/`), recomendada | Python (`host/`), prototipo |
|---|---|---|
| Captura y codificación | Desktop Duplication → procesador de vídeo D3D11 → QuickSync (Media Foundation), todo en la GPU | FFmpeg 6.0 (`ddagrab` + `h264_qsv`), copias GPU→CPU→GPU |
| Cursor | aparte del vídeo (se mueve sin esperar a codificar) | dibujado en el vídeo |
| Keyframes | bajo demanda (recuperación inmediata tras pérdidas) | cada segundo |
| Dependencias | un ejecutable | Python + FFmpeg + OpenSSL |

## Host en Rust

```
cargo build --release --manifest-path host-rs\Cargo.toml
host-rs\target\release\xtendedview.exe
```

Para compilar hace falta el SDK de Windows (viene con las herramientas de compilación de Visual
Studio que usa Rust): de él sale `rc.exe`, que mete el icono en el ejecutable.

Hay que lanzarlo desde la carpeta del proyecto, o indicarla con `--root`, para que encuentre `web\`
y `certs\`. Comparte el certificado con la versión Python (los dos crean la misma CA limitada).
Opciones: `--no-panel`, `--start`, `--hotspot`, `--max-inflight-ms`, `--port`,
`--http-port`, `--list` (monitores) y `-v` (diagnóstico).

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
2. Arranca el host. El banner debe decir «Pantalla adicional: disponible».
3. En el panel, elige **Pantalla adicional** y el lado del ordenador en el que está el
   dispositivo. El host conecta un monitor virtual con la resolución más parecida a la de la
   pantalla del dispositivo, lo coloca en ese lado (centrado), lo captura y lo quita al
   desconectar. Si el host se cierra de golpe, el driver quita el monitor solo en un segundo.

Si ninguno de los modos de serie del driver coincide con tu pantalla (el Fold desplegado, por
ejemplo, es casi cuadrado), el panel ofrece un botón **Registrar resolución exacta**. También se
puede hacer a mano, desde PowerShell como administrador y con el dispositivo desconectado:

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
host-rs\target\release\xtendedview.exe --hotspot
```

(o elige «Red propia» en el panel y pulsa Aplicar; la red se crea al encender).

Con la versión Python, en una consola aparte:

```
powershell -ExecutionPolicy Bypass -File tools\hotspot.ps1
```

Las dos formas crean una red Wi-Fi (WPA2) desde el portátil, sin necesidad de internet, y muestran
el nombre y la contraseña. Conecta el Fold a esa red y abre `https://192.168.137.1:8443/`. La red
existe mientras el host esté encendido (o el script abierto). El nombre (`DIRECT-…`) y la
contraseña son aleatorios y cambian cada vez, así que hay que volver a escanear el QR de la red.
El portátil puede seguir conectado a la Wi-Fi de casa a la vez. Android avisará de que la red no
tiene internet: elige mantener la conexión.

Si el portátil sigue conectado a otra Wi-Fi, la tarjeta tiene que repartirse entre las dos y puede
haber más variación de latencia. Para medir lo mejor posible, desconéctala.

## Por cable (Android, anclaje USB) — experimental

Funciona y es la opción con menos latencia (con el Fold: 34 ms en total frente a 53–68 por Wi-Fi), pero
de momento solo para dispositivos con anclaje USB y hay que activarlo a mano cada vez, así que en el
panel aparece como experimental.

El navegador del móvil no puede usar el USB directamente, así que el cable crea una pequeña red entre
los dos equipos con el **anclaje USB** de Android. Para el navegador es lo mismo que una Wi-Fi, pero
sin interferencias: la red tarda menos de 1 ms y no da tirones.

1. En la pantalla de inicio, elige **Cable USB** (o, ya encendida, «Cable USB» y Aplicar; si conectas
   el móvil estando en otra red, el panel ofrece pasar al cable).
2. Conecta el móvil con un cable USB de datos y actívalo en el móvil: Ajustes → Conexiones →
   Conexión compartida y Módem → **Módem USB** (en Samsung; en otros Android, «Compartir conexión
   por USB» o «Anclaje de USB»). Windows ya trae el driver.
3. En cuanto el ordenador ve la tarjeta del cable, el panel muestra el QR con su dirección en esa red.

Con el anclaje, el móvil comparte también su internet, y Windows suele preferirlo a la Wi-Fi, así que
el ordenador gastaría los datos del móvil. El panel lo detecta y ofrece **Usar la Wi-Fi para
internet** (aviso de administrador): sube la métrica de esa tarjeta para que Windows prefiera la
Wi-Fi. Windows lo recuerda para esa tarjeta y, si no hay otra conexión, sigue pudiendo usar el
móvil.

En modo cable solo se aceptan dispositivos que lleguen por el cable. También reconoce un cable de red
directo (adaptador USB-C a Ethernet en un iPad u otro ordenador, sin router), aunque eso aún no está
probado. `xtendedview.exe --list` muestra las tarjetas de red y cuál toma como cable.

## Seguridad

Pensado para usarlo también en sitios públicos:

- **Apagada**, el host corta cualquier conexión que no venga del propio ordenador: desde fuera no
  se ven ni las páginas ni el certificado. Tampoco hay red propia ni monitor virtual.
- **Red propia**: nombre y contraseña aleatorios en cada encendido. El nombre tiene el formato de
  Wi-Fi Direct (`DIRECT-…`, como impresoras y teles), así que no dice de quién es el ordenador ni
  deja seguirlo de un sitio a otro. La contraseña tiene 16 caracteres (80 bits), suficiente aunque
  alguien capture la conexión e intente adivinarla después.
- **Solo por las conexiones activas**: cada conexión llega por su tarjeta (la red propia, la del
  cable o la del ordenador). Si solo está activa la red propia, por ejemplo, los equipos de la Wi-Fi
  de una cafetería en la que siga el portátil no pueden ni conectarse.
- **Red del ordenador**: cualquiera de esa red llega al host, y lo protegen el código y la
  aceptación en el panel. En una Wi-Fi pública es mejor la red propia o el cable.
- **Aceptar cada dispositivo**: aunque alguien tenga el código, no ve nada hasta que lo aceptes en
  el panel. Los aceptados se olvidan al apagar.
- **Código de emparejamiento**: 8 caracteres (31⁸ combinaciones), nuevo en cada encendido. Se
  comprueba un intento cada vez y cada fallo hace esperar un segundo, aunque se abran muchas
  conexiones a la vez (como mucho un intento fallido por segundo).
- La imagen y el control solo viajan cifrados (HTTPS). Sin cifrar solo se sirve lo necesario para
  instalar el certificado, sin el nombre del equipo.
- El panel de control solo responde desde el propio ordenador.
- **Certificado del ordenador (CA)**, el que se instala en los dispositivos: está limitado a
  direcciones privadas y a `localhost` (restricciones de nombre críticas, comprobadas con OpenSSL y
  con Windows). Aunque alguien robara su clave de `certs\`, no le serviría para suplantar ninguna web
  de internet en esos dispositivos. Tampoco puede crear otras CA y no lleva el nombre del equipo:
  se llama «XtendedView - CA local XXXX», con un identificador al azar.

Las CA de versiones anteriores («Pantalla PoC CA (equipo)», sin restricciones, y «Pantalla secundaria
- CA local …», con el nombre antiguo) se rehacen solas al arrancar, así que hay que **volver a
instalar el certificado** desde `/setup` y **borrar el anterior** de cada dispositivo. La página
`/setup` explica cómo.

## Opciones

| Opción | Por defecto | Para qué |
|---|---|---|
| `--encoder qsv\|x264` | `qsv` | QuickSync (hardware) o x264 (CPU) |
| `--pipeline cpu\|gpu` | `cpu` | dónde se escala la imagen antes de codificar (solo con QuickSync) |
| `--monitor N` | `0` | monitor a capturar |
| `--gop S` | `1.0` | segundos entre keyframes (tiempo máximo de recuperación tras una pérdida) |
| `--max-inflight-ms MS` | `200` | retraso acumulado a partir del cual se descartan frames en vez de acumular retraso |
| `--port`, `--http-port` | `8443`, `8080` | puertos HTTPS y HTTP |

Estas opciones son de la versión Python. En la de Rust, los fotogramas (30–120), la calidad
(10–50 Mbps) y la resolución (100/75/50 %) se eligen en el panel del ordenador.

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
- Cable: Android con anclaje USB; el Ethernet directo (iPad u otro PC con adaptador) aún sin probar.
  Un iPad solo Wi-Fi no puede hacer anclaje USB: necesita el adaptador Ethernet.

## Estructura

```
host-rs/src/       host en Rust: capture.rs (DDA y cursor), convert.rs (BGRA→NV12 en GPU),
                   mfenc.rs (QuickSync), pipeline.rs (hilo de captura), server.rs (HTTPS/WS, panel,
                   encendido y seguridad), vdd.rs + display.rs (monitor virtual), certs.rs,
                   input.rs (táctil, ratón y lápiz), hotspot.rs (red propia), prefs.rs (ajustes),
                   sys.rs (red, QR, ventanas), h264.rs
host-rs/build.rs + xtendedview.rc   icono y nombre del ejecutable (con rc.exe del SDK de Windows)
assets/            icono de XtendedView (xtendedview.png de 1024 px y xtendedview.ico) y logotipo de
                   texto para tema oscuro y claro (logo-tema-*.png)
tools/iconos.ps1   genera todos los iconos a partir de una imagen (después, recompilar)
tools/logo.ps1     prepara el logotipo de texto (sobre fondo negro) para el panel
tools/vdd-resoluciones.ps1  resoluciones propias del monitor virtual (administrador)
host/main.py       servidor HTTPS/WebSocket, sesión del visor, control de flujo y estadísticas
host/encoder.py    comando de FFmpeg y lectura de la salida FLV frame a frame
host/websocket.py  WebSocket mínimo (RFC 6455)
host/certs.py      autoridad de certificación local y certificado con las IP del portátil
host/winapi.py     DPI, monitores e inyección de táctil/ratón (ctypes)
web/               visor (index.html, viewer.js), panel del ordenador (panel.*), sonda (probe.*),
                   configuración (setup.html) e iconos (favicon.ico, icon-*.png)
tools/hotspot.ps1  red Wi-Fi directa del portátil (Wi-Fi Direct en modo legacy)
```
