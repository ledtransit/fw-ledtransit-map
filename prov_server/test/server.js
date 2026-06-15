const express = require("express");

const host = process.env.HOST || "localhost";
const port = process.env.PORT || 3002;

const app = express();

// Serve public
app.use(express.static("../public"));

// POST route for /api/scan-wifi -> 200 OK JSON [{ ssid: string, rssi: number, open: 0 | 1 }]
app.post("/api/scan-wifi", async (req, res) => {
  // Simulate scanning WiFi networks
  const apList = [
    { ssid: "Network1", rssi: -40, open: 0 },
    { ssid: "Network2 Long Name Example Test", rssi: -60, open: 0 },
    { ssid: "Network3", rssi: -80, open: 0 },
    { ssid: "Network4", rssi: -50, open: 1 },
    { ssid: "Network5", rssi: -70, open: 0 },
    { ssid: "Network1", rssi: -40, open: 0 },
    { ssid: "Network2", rssi: -60, open: 1 },
    { ssid: "Network3", rssi: -80, open: 0 },
    { ssid: "Network4", rssi: -50, open: 1 },
    { ssid: "Network5", rssi: -70, open: 0 },
  ];
  // Simulate network delay
  await new Promise((resolve) => setTimeout(resolve, 1000));
  res.json(apList);
});

// POST route for /api/connect-wifi -> 200 OK ()
app.post("/api/connect-wifi", express.json(), async (req, res) => {
  const { ssid, password } = req.body;
  console.log(`Received WiFi credentials: SSID=${ssid}, Password=${password}`);
  await new Promise((resolve) => setTimeout(resolve, 1000));
  res.sendStatus(200);
});

// POST route for /api/identify -> 200 OK ()
app.post("/api/identify", async (req, res) => {
  console.log("Identify request received");
  await new Promise((resolve) => setTimeout(resolve, 1000));
  res.sendStatus(200);
});

// Start the server
app.listen(port, () => {
  console.log(`Server running at http://${host}:${port}/setup-wifi.html?tok=abc123`);
});
