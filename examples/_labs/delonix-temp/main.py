"""Delonix Temp — a tiny FastAPI weather API. Real-time temperature for any
city, via Open-Meteo (no API key needed). Built to be the capstone example
for the Delonix docs: build -> run -> httproute -> tunnel -> a real public URL.
"""
import httpx
from fastapi import FastAPI, HTTPException
from fastapi.responses import HTMLResponse

app = FastAPI(title="Delonix Temp")

GEOCODE_URL = "https://geocoding-api.open-meteo.com/v1/search"
FORECAST_URL = "https://api.open-meteo.com/v1/forecast"


@app.get("/health")
async def health():
    return {"status": "ok"}


@app.get("/api/weather/{city}")
async def weather(city: str):
    async with httpx.AsyncClient(timeout=8.0) as client:
        geo = await client.get(GEOCODE_URL, params={"name": city, "count": 1})
        geo.raise_for_status()
        results = geo.json().get("results")
        if not results:
            raise HTTPException(status_code=404, detail=f"city not found: {city}")
        place = results[0]

        fc = await client.get(
            FORECAST_URL,
            params={
                "latitude": place["latitude"],
                "longitude": place["longitude"],
                "current": "temperature_2m,weather_code",
            },
        )
        fc.raise_for_status()
        current = fc.json()["current"]

    return {
        "city": place["name"],
        "country": place.get("country"),
        "temperature_c": current["temperature_2m"],
        "observed_at": current["time"],
    }


PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Delonix Temp</title>
<style>
:root{color-scheme:light dark}
body{font-family:-apple-system,'Segoe UI',Roboto,sans-serif;max-width:32rem;margin:4rem auto;
padding:0 1.5rem;text-align:center}
h1{font-size:1.6rem;margin-bottom:.2rem}
.temp{font-size:4.5rem;font-weight:700;margin:1.2rem 0 .2rem}
.city{font-size:1.2rem;color:#888}
.meta{font-size:.85rem;color:#888;margin-top:1.5rem}
input,button{font-size:1rem;padding:.4rem .7rem;border-radius:6px;border:1px solid #888}
button{cursor:pointer}
footer{margin-top:3rem;font-size:.8rem;color:#888}
footer a{color:inherit}
</style>
</head>
<body>
<h1>&#9728;&#65039; Delonix Temp</h1>
<p class="city" id="city">a carregar&hellip;</p>
<div class="temp" id="temp">&mdash;</div>
<form id="f"><input id="q" value="Luanda"><button>ver</button></form>
<p class="meta" id="meta"></p>
<footer>Servido por <a href="https://github.com/angolardevops/delonix-runtime">Delonix Runtime</a>
via <code>container run</code> + <code>httproute</code> + <code>tunnel</code>. Actualiza a cada 30s.</footer>
<script>
let city = "Luanda";
async function refresh() {
  document.getElementById("city").textContent = city + "…";
  try {
    const r = await fetch("/api/weather/" + encodeURIComponent(city));
    if (!r.ok) throw new Error(await r.text());
    const d = await r.json();
    document.getElementById("city").textContent = d.city + (d.country ? ", " + d.country : "");
    document.getElementById("temp").textContent = d.temperature_c + "°C";
    document.getElementById("meta").textContent = "observado " + d.observed_at + " UTC";
  } catch (e) {
    document.getElementById("temp").textContent = "?";
    document.getElementById("meta").textContent = "erro: " + e.message;
  }
}
document.getElementById("f").addEventListener("submit", (e) => {
  e.preventDefault();
  city = document.getElementById("q").value || "Luanda";
  refresh();
});
refresh();
setInterval(refresh, 30000);
</script>
</body>
</html>"""


@app.get("/", response_class=HTMLResponse)
async def index():
    return PAGE
