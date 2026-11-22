// Enhancements only: without this script the tabs still switch and the terminal shows its whole run.

// Pick the install command for this OS.
const platform = navigator.userAgentData?.platform ?? navigator.platform ?? "";
if (/^win/i.test(platform)) document.getElementById("os-windows").checked = true;

// Copy the visible command.
const copy = document.querySelector(".copy");
copy.hidden = false;
copy.addEventListener("click", async () => {
	const windows = document.getElementById("os-windows").checked;
	const command = document.querySelector(windows ? ".windows .sel" : ".unix .sel");
	try {
		await navigator.clipboard.writeText(command.textContent);
		copy.textContent = "Copied";
	} catch {
		// Clipboard access denied: select the command so the reader can copy it.
		getSelection().selectAllChildren(command);
		copy.textContent = "Selected";
	}
	setTimeout(() => (copy.textContent = "Copy"), 1500);
});

// Play the recorded run once it scrolls into view, unless the reader prefers less motion.
// While it plays, the button skips to the whole run (WCAG 2.2.2); after, it replays.
const term = document.querySelector(".term");
const button = document.querySelector(".replay");
if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
	const play = () => {
		term.classList.remove("wait", "play");
		void term.offsetWidth; // restart the CSS animations
		term.classList.add("play");
		button.textContent = "Skip";
		button.hidden = false;
	};
	term.classList.add("wait");
	new IntersectionObserver((entries, observer) => {
		if (!entries[0].isIntersecting) return;
		observer.disconnect();
		play();
	}, { threshold: 0.35 }).observe(term);
	// The last step's reveal ends the run.
	term.querySelector(".step:last-child").addEventListener("animationend", () => (button.textContent = "Replay"));
	button.addEventListener("click", () => {
		if (button.textContent !== "Skip") return play();
		term.classList.remove("wait", "play");
		button.textContent = "Replay";
	});
}
