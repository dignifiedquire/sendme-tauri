import { useState } from "react";
import reactLogo from "./assets/react.svg";
import { invoke } from "@tauri-apps/api/tauri";
import "./App.css";
import { listen } from '@tauri-apps/api/event'



function App() {
  const [uploadMsg, setUploadMsg] = useState("");

  listen('tauri://file-drop', event => {
    console.log(uploadMsg, event);
    
    if (!uploadMsg) {
      invoke("upload", { file: event.payload[0] })
      .then(msg => setUploadMsg(msg));
    }
  });

  let output = <div>Drop it</div>;
  if (uploadMsg) {
    output = (
      <div className="block">
          { uploadMsg }
      </div>
    );
  }

  return (
    <div className="container">
      <h1>Share with SendMe!</h1>
      {output}
    </div>
  );
}

export default App;
