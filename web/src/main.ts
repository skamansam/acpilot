import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';

const target = document.getElementById('app');
if (!target) {
  throw new Error('the page is missing #app; the built HTML and this entry point disagree');
}

export default mount(App, { target });
